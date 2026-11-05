@tool
extends RefCounted
## `call_method`, an editing call on an object the edit owns, and the property
## paths every batch operation uses to reach into the resources a scene embeds.
##
## Some edits have no property to set: painting a TileMapLayer, keying an
## Animation, adding a state to a state machine. A batch makes them by calling
## one of the methods below on a node of the scene, or on a resource the scene
## embeds that a `resource` selector picks: a node property, or a property
## path through embedded resources such as `tree_root:states/Run/node`, an
## animation or an animation library of an AnimationPlayer or AnimationTree. A
## resource saved in its own file is refused: editing it here would change
## that file too.
##
## Every listed method changes only its receiver's stored state, so a call is
## undone by putting that state back: the stored properties the call changed
## are written back, or, for a class whose calls add or remove stored
## properties, the receiver is emptied the way CLEARING says and every stored
## property written back. A graph resource in REPLACING is not changed at all:
## the call is made on a rebuilt copy of the outermost embedded resource it
## reached, the node then holds the copy, and undo gives it the untouched
## original back. A class in SETTLE brings its stored properties up to date
## with that method after the calls and after each undo and redo. A class in
## BAKING is a navigation region whose bake method rebuilds the resource the
## property it names holds: it is called once, never on a thread, on a rebuilt
## copy of that resource, which the region then holds, and undo gives the
## region the original back. Arguments are checked against the method's engine
## signature before any call.

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
	"Control": ["set_anchors_preset", "set_anchors_and_offsets_preset", "set_offsets_preset", "set_anchor", "set_anchor_and_offset"],
	"AnimationNodeStateMachine": ["add_node", "remove_node", "rename_node", "replace_node", "set_node_position", "add_transition", "remove_transition", "remove_transition_by_index", "set_graph_offset"],
	"AnimationNodeBlendTree": ["add_node", "remove_node", "rename_node", "connect_node", "disconnect_node", "set_node_position", "set_graph_offset"],
	"AnimationNodeBlendSpace1D": ["add_blend_point", "remove_blend_point", "set_blend_point_position", "set_blend_point_node"],
	"AnimationNodeBlendSpace2D": ["add_blend_point", "remove_blend_point", "set_blend_point_position", "set_blend_point_node", "add_triangle", "remove_triangle"],
	"VisualShader": ["add_node", "remove_node", "connect_nodes", "disconnect_nodes", "set_node_position"],
	"TileSet": ["add_source", "remove_source", "add_physics_layer", "remove_physics_layer", "set_physics_layer_collision_layer", "set_physics_layer_collision_mask", "add_terrain_set", "remove_terrain_set", "set_terrain_set_mode", "add_terrain", "remove_terrain", "set_terrain_name", "set_terrain_color", "add_navigation_layer", "remove_navigation_layer", "add_custom_data_layer", "remove_custom_data_layer", "set_custom_data_layer_name", "set_custom_data_layer_type"],
	"TileSetAtlasSource": ["create_tile", "remove_tile", "create_alternative_tile", "remove_alternative_tile", "clear_tiles_outside_texture"],
	"NavigationPolygon": ["add_outline", "add_outline_at_index", "set_outline", "remove_outline", "clear_outlines", "add_polygon", "clear_polygons", "clear"],
	"NavigationMesh": ["add_polygon", "clear_polygons", "create_from_mesh", "clear"],
	"MeshLibrary": ["create_item", "set_item_mesh", "set_item_name", "set_item_mesh_transform", "set_item_shapes", "set_item_navigation_mesh", "remove_item", "clear"],
	"NavigationRegion2D": ["bake_navigation_polygon"],
	"NavigationRegion3D": ["bake_navigation_mesh"],
}
const CLEARING := {"TileMapLayer": ["clear"], "AnimationMixer": ["get_animation_library_list", "remove_animation_library"], "Animation": ["clear"], "Theme": ["clear"]}
const SETTLE := {"TileMapLayer": "update_internals"}
const REPLACING := ["AnimationNodeStateMachine", "AnimationNodeBlendTree", "AnimationNodeBlendSpace1D", "AnimationNodeBlendSpace2D", "VisualShader", "TileSet", "TileSetAtlasSource", "MeshLibrary"]
const BAKING := {"NavigationRegion2D": "navigation_polygon", "NavigationRegion3D": "navigation_mesh"}
const NEW_RESOURCE_FORBIDDEN := ["Script", "GDExtension"]
const MAX_CALL_ARGUMENTS := 16
const MAX_REPEATED_CALLS := 4096


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


## Whether `object` lists `property`, a dynamic property such as a state
## machine's `states/Run/node` included.
static func has_property(object: Object, property: String) -> bool:
	for entry in object.get_property_list():
		if str(entry["name"]) == property:
			return true
	return false


## A new object of the class `name` inheriting `base`: an engine class, or a
## global script class (`class_name`) whose engine base inherits `base`, with
## its script attached. Answers {"ok": true, "object"}, or {"ok": false,
## "detail"}.
static func instantiate(name: String, base: String) -> Dictionary:
	if ClassDB.class_exists(name):
		if ClassDB.can_instantiate(name) and (name == base or ClassDB.is_parent_class(name, base)):
			return {"ok": true, "object": ClassDB.instantiate(name)}
		return {"ok": false, "detail": "%s is not a %s class that can be created" % [name, base]}
	var path := ""
	for entry in ProjectSettings.get_global_class_list():
		if str(entry.get("class", "")) == name:
			path = str(entry.get("path", ""))
	if path == "":
		return {"ok": false, "detail": "%s names neither an engine class nor a global script class" % name}
	var script = ResourceLoader.load(path, "Script")
	if not (script is Script):
		return {"ok": false, "detail": "the script of %s, %s, cannot be loaded" % [name, path]}
	var native := String(script.get_instance_base_type())
	if not ClassDB.can_instantiate(native) or not (native == base or ClassDB.is_parent_class(native, base)):
		return {"ok": false, "detail": "%s extends %s, not %s" % [name, native, base]}
	var object: Object = ClassDB.instantiate(native)
	object.set_script(script)
	if object.get_script() != script:
		if not (object is RefCounted):
			object.free()
		return {"ok": false, "detail": "%s could not be given its script %s" % [name, path]}
	return {"ok": true, "object": object}


## Where the property path `path` leads from `base`. Its names are separated
## by ":"; each but the last that names a property holding a resource passes
## into it, which must be one `owner_path` embeds and not a script. With
## `to_resource` every name must pass into such a resource. Answers {"ok":
## true, "holder", "leaf", "outer_property", "outer", "inner"}: `leaf` is what
## remains of the path on `holder` (a property, then components of its value,
## as `position:x`), empty with `to_resource`; `outer` is the first resource
## passed, held by `base`'s property `outer_property`, and `inner` the path
## from it to `holder`. `outer` is null when the path passes no resource.
static func walk(base: Object, path: String, owner_path: String, to_resource: bool = false) -> Dictionary:
	var parts: PackedStringArray = path.split(":")
	for part in parts:
		if part == "":
			return _refusal("invalid_property_path", "%s has an empty name" % path)
	var hops := parts.size() if to_resource else parts.size() - 1
	var holder: Object = base
	var at := 0
	var outer: Resource = null
	while at < hops:
		var passed := ":".join(parts.slice(0, at + 1))
		if not has_property(holder, parts[at]):
			if to_resource:
				return _refusal("unknown_property", passed)
			break
		var value = holder.get(parts[at])
		if not (value is Object):
			if to_resource:
				return _refusal("not_a_resource", "%s holds no resource" % passed)
			break
		if not (value is Resource):
			return _refusal("not_a_resource", "%s holds an object that is not a resource" % passed)
		if value is Script:
			return _refusal("script_resource", "%s is a script; change scripts with script.patch" % passed)
		if not embedded(value, owner_path):
			return _refusal("external_resource", "%s is the file %s; edit it with resource.batch.apply" % [passed, value.resource_path])
		if outer == null:
			outer = value
		holder = value
		at += 1
	return {
		"ok": true,
		"holder": holder,
		"leaf": ":".join(parts.slice(at)),
		"outer_property": parts[0] if outer != null else "",
		"outer": outer,
		"inner": ":".join(parts.slice(1, at)) if outer != null else "",
	}


## Why the leaf of a walk cannot be written, as {"reason", "detail"}, or {}:
## a leaf with components names an existing component of the value of a
## property the holder lists, and only a whole property is cleared.
static func leaf_problem(holder: Object, leaf: String, clear: bool) -> Dictionary:
	if not leaf.contains(":"):
		return {}
	var name := leaf.get_slice(":", 0)
	if clear:
		return {"reason": "invalid_property_path", "detail": "clear_property clears a whole property, not %s" % leaf}
	if not has_property(holder, name):
		return {"reason": "unknown_property", "detail": "a %s has no property %s" % [holder.get_class(), name]}
	if holder.get(name) == null:
		return {"reason": "unknown_property", "detail": "%s holds nothing; set it first" % name}
	if holder.get_indexed(NodePath(leaf)) == null:
		return {"reason": "unknown_property", "detail": "%s names no part of %s" % [leaf, name]}
	return {}


## The value `clear_property` writes to `holder`'s property `name`.
static func cleared_value(holder: Object, name: String):
	if holder.property_can_revert(name):
		return holder.property_get_revert(name)
	return ClassDB.class_get_property_default_value(holder.get_class(), name)


## Whether a value read back as `after` is a write of `value` that the
## property took: the same object, null, or a value of the same type, with
## either numeric type for a number and either string type for a string.
## Whether the engine kept exactly the value written, for the types whose
## values compare exactly. A setter may replace a value of the right type, as a
## player keeps `Master` for a bus the layout lacks; floats and the types built
## of them are compared by type only, since the engine stores them rounded.
static func kept(value, after) -> bool:
	match typeof(value):
		TYPE_BOOL, TYPE_STRING, TYPE_STRING_NAME:
			return str(after) == str(value)
		TYPE_INT:
			return (typeof(after) == TYPE_INT or typeof(after) == TYPE_FLOAT) and float(after) == float(value)
	return true


static func took(value, after) -> bool:
	match typeof(value):
		TYPE_NIL:
			return after == null
		TYPE_OBJECT:
			return typeof(after) == TYPE_OBJECT and after == value
		TYPE_FLOAT, TYPE_INT:
			return typeof(after) == TYPE_FLOAT or typeof(after) == TYPE_INT
		TYPE_STRING, TYPE_STRING_NAME:
			return typeof(after) == TYPE_STRING or typeof(after) == TYPE_STRING_NAME
	return typeof(after) == typeof(value)


## The object a scene's `call_method` calls: `node`, or the resource of `node`
## the `selector` names, which the scene file at `owner_path` must embed. A
## `property` selector may be a property path through embedded resources.
## Answers {"ok": true, "object", "outer_property", "outer", "inner"} as
## `walk` does, `outer` null unless a property selector passed a resource.
static func receiver(node: Node, selector, owner_path: String) -> Dictionary:
	if selector == null:
		return {"ok": true, "object": node, "outer_property": "", "outer": null, "inner": ""}
	if typeof(selector) != TYPE_DICTIONARY or selector.size() != 1:
		return _refusal("invalid_selector", "`resource` names one of property, animation, library")
	var kind := str(selector.keys()[0])
	var name := str(selector[kind])
	var resource = null
	match kind:
		"property":
			var found: Dictionary = walk(node, name, owner_path, true)
			if not found["ok"]:
				return found
			return {"ok": true, "object": found["holder"], "outer_property": found["outer_property"],
				"outer": found["outer"], "inner": found["inner"]}
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
	return {"ok": true, "object": resource, "outer_property": "", "outer": null, "inner": ""}


## Whether the class `name` may be created by a `new_resource` argument.
static func new_resource_class_problem(name: String) -> String:
	for forbidden in NEW_RESOURCE_FORBIDDEN:
		if name == forbidden or ClassDB.class_exists(name) and ClassDB.is_parent_class(name, forbidden):
			return "a new resource cannot be a %s" % forbidden
	return ""


## One argument: a new embedded resource, or a tagged value.
static func decode_argument(tagged) -> Dictionary:
	if typeof(tagged) == TYPE_DICTIONARY and str(tagged.get("type", "")) == "new_resource":
		var value = tagged.get("value")
		if tagged.size() != 2 or typeof(value) != TYPE_DICTIONARY or not value.has("class") \
				or value.size() > (2 if value.has("properties") else 1):
			return {"ok": false, "reason": "a new resource is {type: new_resource, value: {class, properties?}}"}
		var resource_class := str(value["class"])
		var problem := new_resource_class_problem(resource_class)
		if problem != "":
			return {"ok": false, "reason": problem}
		var made: Dictionary = instantiate(resource_class, "Resource")
		if not made["ok"]:
			return {"ok": false, "reason": made["detail"]}
		var resource: Resource = made["object"]
		var properties = value.get("properties", {})
		if typeof(properties) != TYPE_DICTIONARY:
			return {"ok": false, "reason": "a new resource's properties are an object"}
		for key in properties:
			var name := str(key)
			if not has_property(resource, name):
				return {"ok": false, "reason": "a %s has no property %s" % [resource_class, name]}
			var decoded: Dictionary = VariantCodec.decode(properties[key])
			if not decoded["ok"]:
				return {"ok": false, "reason": decoded["reason"]}
			resource.set(name, decoded["value"])
			if not took(decoded["value"], resource.get(name)):
				return {"ok": false, "reason": "%s of a %s is a %s" % [name, resource_class, type_string(typeof(resource.get(name)))]}
			if not kept(decoded["value"], resource.get(name)):
				return {"ok": false, "reason": "%s of a %s kept %s, not %s" % [name, resource_class, str(resource.get(name)), str(decoded["value"])]}
		return {"ok": true, "value": resource}
	return VariantCodec.decode(tagged)


## The argument lists of a `call_method`: its `arguments`, or each list of its
## `repeat`, decoded. Answers {"ok": true, "lists"} or a refusal.
static func argument_lists(op: Dictionary) -> Dictionary:
	var given: Array = []
	if op.has("repeat"):
		var repeat = op["repeat"]
		if op.has("arguments") or typeof(repeat) != TYPE_ARRAY or repeat.is_empty() or repeat.size() > MAX_REPEATED_CALLS:
			return _refusal("invalid_arguments", "`repeat` lists 1 to %d argument lists, without `arguments`" % MAX_REPEATED_CALLS)
		given = repeat
	else:
		given = [op.get("arguments", [])]
	var lists: Array = []
	for arguments in given:
		if typeof(arguments) != TYPE_ARRAY or arguments.size() > MAX_CALL_ARGUMENTS:
			return _refusal("invalid_arguments", "a call takes at most %d arguments" % MAX_CALL_ARGUMENTS)
		var decoded_list: Array = []
		for argument in arguments:
			var decoded: Dictionary = decode_argument(argument)
			if not decoded["ok"]:
				return _refusal("invalid_value", decoded["reason"])
			decoded_list.append(decoded["value"])
		lists.append(decoded_list)
	return {"ok": true, "lists": lists}


## The engine's description of `method` on `object`, or {} when it has none.
static func method_info(object: Object, method: String) -> Dictionary:
	for info in ClassDB.class_get_method_list(object.get_class()):
		if str(info["name"]) == method:
			return info
	return {}


## Why `arguments` do not fit `method`'s engine signature `info` on `object`,
## or "".
static func signature_problem(object: Object, method: String, info: Dictionary, arguments: Array) -> String:
	if info.is_empty():
		return "%s has no method %s" % [object.get_class(), method]
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


## A new resource of `resource`'s class and script whose stored properties
## are set in the order the resource lists them, as loading its file sets
## them: every resource `owner_path` embeds in it, at any depth, is rebuilt
## the same way, and one embedded twice is rebuilt once. Scripts and resource
## files are kept, not copied.
static func rebuilt(resource: Resource, owner_path: String, memo: Dictionary = {}) -> Resource:
	var id := resource.get_instance_id()
	if memo.has(id):
		return memo[id]
	var copy: Resource = ClassDB.instantiate(resource.get_class())
	memo[id] = copy
	if resource.get_script() != null:
		copy.set_script(resource.get_script())
	for property in resource.get_property_list():
		if not (int(property.get("usage", 0)) & PROPERTY_USAGE_STORAGE):
			continue
		var name := str(property["name"])
		if name == "script" or name == "resource_path":
			continue
		copy.set(name, _rebuilt_value(resource.get(name), owner_path, memo))
	return copy


static func _rebuilt_value(value, owner_path: String, memo: Dictionary):
	if value is Resource:
		if value is Script or not embedded(value, owner_path):
			return value
		return rebuilt(value, owner_path, memo)
	if value is Array:
		var array: Array = value.duplicate(false)
		for index in array.size():
			array[index] = _rebuilt_value(array[index], owner_path, memo)
		return array
	if value is Dictionary:
		var dictionary: Dictionary = value.duplicate(false)
		for key in dictionary:
			dictionary[key] = _rebuilt_value(dictionary[key], owner_path, memo)
		return dictionary
	return value


## Every resource embedded in `value`, `value` itself included, by instance
## id, into `into`.
static func embedded_resources(value, owner_path: String, into: Dictionary) -> void:
	if value is Resource:
		if value is Script or not embedded(value, owner_path) or into.has(value.get_instance_id()):
			return
		into[value.get_instance_id()] = true
		for property in value.get_property_list():
			if int(property.get("usage", 0)) & PROPERTY_USAGE_STORAGE and str(property["name"]) != "script":
				embedded_resources(value.get(str(property["name"])), owner_path, into)
	elif value is Array:
		for item in value:
			embedded_resources(item, owner_path, into)
	elif value is Dictionary:
		for key in value:
			embedded_resources(value[key], owner_path, into)


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


## Call `method` of the table's class `listed` on `receiver` once with each of
## `lists`, and work out how to redo and undo the calls together from the
## receiver's stored properties before and after. Answers {"ok": true,
## "values", "do", "undo"}, or {"ok": false, "unresolved", "detail"} when the
## calls cannot be undone; the receiver is then put back, and `unresolved`
## says that putting it back did not work.
static func call_recorded(receiver: Object, listed: String, method: String, lists: Array) -> Dictionary:
	var recipe: Array = CLEARING.get(listed, [])
	var settle: String = SETTLE.get(listed, "")
	var before: Dictionary = stored(receiver)
	var clear_before: Array = clearing_calls(receiver, recipe) if not recipe.is_empty() else []
	var values: Array = []
	for arguments in lists:
		values.append(receiver.callv(method, arguments))
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
	return {"ok": true, "values": values, "do": do_calls, "undo": undo_calls}


## The tagged spelling of a call's result, or null when it returned nothing.
static func encode_result(value):
	if value == null:
		return null
	var encoded: Dictionary = VariantCodec.encode(value)
	if encoded["ok"]:
		return encoded["value"]
	return {"type": "unrepresentable", "value": {"variant_type": type_string(typeof(value))}}


## What one `call_method` returned: `value` for one call, `values` in order
## for a `repeat`.
## What a bake produced in `held`, a navigation mesh or polygon.
static func baked(held: Resource) -> Dictionary:
	var report := {
		"class": held.get_class(),
		"polygons": int(held.get_polygon_count()),
		"vertices": int(held.get_vertices().size()),
	}
	if held is NavigationPolygon:
		report["outlines"] = int(held.get_outline_count())
	return report


static func result_entry(index: int, method: String, values: Array, repeated: bool) -> Dictionary:
	if not repeated:
		return {"index": index, "method": method, "value": encode_result(values[0])}
	var encoded: Array = []
	for value in values:
		encoded.append(encode_result(value))
	return {"index": index, "method": method, "values": encoded}
