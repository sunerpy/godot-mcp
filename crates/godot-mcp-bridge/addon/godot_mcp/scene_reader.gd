@tool
extends RefCounted
## `editor.scene.read`: the reading methods of the table below, called on
## nodes of the scene the editor is editing, or on resources they hold, as the
## editor holds them, unsaved edits included. Each value is answered in
## Godot's own text spelling, as the headless helper answers `scene.read` for
## the saved file, so the server reads both alike.
##
## A read changes nothing: it adds no undo history and marks nothing unsaved.
## Reading a Gradient sorts its points, which could reorder what it stores, so
## a Gradient is read through a copy; and the scene's semantic digest is taken
## before and after, so a read that changed the scene anyway says so instead
## of answering.
##
## What the editor logged while a call ran is that call's: its errors,
## warnings and script and shader errors, from the log capture of Godot 4.5.1
## and later, up to MAX_CALL_LOGGED. Another editor thread may log at the
## same moment, so an entry may not be the call's own; nothing a call raised is
## left out. Godot 4.4.1 cannot hand its log to a plugin, so each of its calls
## says its log is not complete.

const VariantCodec = preload("variant_codec.gd")
const EditingMethods = preload("editing_methods.gd")

# The reading methods, as crates/godot-mcp-domain/src/reading_methods.rs lists
# them; a test holds this copy to that table. Change the table, not this copy.
const READING_METHODS := {
	"TileMapLayer": ["get_cell_source_id", "get_cell_atlas_coords", "get_cell_alternative_tile", "get_used_cells", "get_used_cells_by_id", "get_used_rect"],
	"GridMap": ["get_cell_item", "get_cell_item_orientation", "get_used_cells", "get_used_cells_by_item"],
	"Line2D": ["get_point_count", "get_point_position"],
	"Skeleton3D": ["get_bone_count", "get_bone_name", "find_bone", "get_bone_parent", "get_bone_rest"],
	"AnimationMixer": ["get_animation_library_list", "get_animation_list", "has_animation", "has_animation_library"],
	"AnimationLibrary": ["get_animation_list", "has_animation"],
	"Animation": ["get_track_count", "find_track", "track_get_path", "track_get_type", "track_is_enabled", "track_get_interpolation_type", "track_get_interpolation_loop_wrap", "value_track_get_update_mode", "track_get_key_count", "track_get_key_time", "track_get_key_value", "track_get_key_transition", "track_find_key", "get_length", "get_loop_mode", "get_step"],
	"SpriteFrames": ["get_animation_names", "has_animation", "get_frame_count", "get_frame_duration", "get_animation_speed", "get_animation_loop"],
	"Curve": ["get_point_count", "get_point_position", "get_point_left_tangent", "get_point_right_tangent", "sample"],
	"Curve2D": ["get_point_count", "get_point_position", "get_point_in", "get_point_out", "get_baked_length"],
	"Curve3D": ["get_point_count", "get_point_position", "get_point_in", "get_point_out", "get_point_tilt", "get_baked_length"],
	"Gradient": ["get_point_count", "get_offset", "get_color", "sample"],
	"Theme": ["get_type_list", "get_color_list", "get_constant_list", "get_font_size_list", "get_font_list", "get_icon_list", "get_stylebox_list", "get_color", "get_constant", "get_font_size", "has_color", "has_constant", "has_font_size", "has_font", "has_icon", "has_stylebox"],
	"AnimationNodeStateMachine": ["get_node_list", "has_node", "get_node_position", "get_transition_count", "get_transition_from", "get_transition_to", "get_graph_offset"],
	"AnimationNodeBlendTree": ["get_node_list", "has_node", "get_node_position", "get_graph_offset"],
	"AnimationNodeBlendSpace1D": ["get_blend_point_count", "get_blend_point_position"],
	"AnimationNodeBlendSpace2D": ["get_blend_point_count", "get_blend_point_position", "get_triangle_count", "get_triangle_point"],
	"VisualShader": ["get_node_list", "get_node_position", "get_node_connections", "get_valid_node_id"],
	"TileSet": ["get_source_count", "get_source_id", "has_source", "get_physics_layers_count", "get_terrain_sets_count", "get_terrains_count", "get_terrain_name", "get_navigation_layers_count", "get_custom_data_layers_count", "get_custom_data_layer_name"],
	"TileSetAtlasSource": ["get_tiles_count", "get_tile_id", "has_tile", "get_atlas_grid_size", "get_alternative_tiles_count"],
	"NavigationPolygon": ["get_polygon_count", "get_polygon", "get_vertices", "get_outline_count", "get_outline"],
	"NavigationMesh": ["get_polygon_count", "get_polygon", "get_vertices"],
	"MeshLibrary": ["get_item_list", "get_item_name", "get_last_unused_item_id"],
	"Mesh": ["get_surface_count", "get_aabb"],
	"Shader": ["get_mode", "get_shader_uniform_list"],
}
const MAX_READ_CALLS := 64
const MAX_READ_VALUE_BYTES := 262144

## Most log entries one call answers; past them the call says its log is not
## complete.
const MAX_CALL_LOGGED := 64
const LOGGED_KINDS := ["error", "warning", "script", "shader"]


static func _fail(kind: String, message: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": "not_started"}


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "not_started"}


static func _refusal(reason: String, detail: String) -> Dictionary:
	return {"ok": false, "reason": reason, "detail": detail}


static func _unread(entry: Dictionary, kind: String, detail: String) -> Dictionary:
	entry["error_kind"] = kind
	entry["detail"] = detail.replace("\n", " ")
	return entry


static func read(handlers, payload: Dictionary) -> Dictionary:
	var calls = payload.get("calls")
	if typeof(calls) != TYPE_ARRAY or calls.is_empty() or calls.size() > MAX_READ_CALLS:
		return _fail("invalid_input", "`calls` lists 1 to %d calls" % MAX_READ_CALLS)
	var found: Dictionary = handlers.open_scene_root(str(payload.get("scene", "")))
	if not found.get("ok", false):
		return found
	var root: Node = found["root"]
	var capture = handlers.editor_log if handlers.log_capture() else null
	var before: String = handlers.edit_digest(root)
	var answered: Array = []
	for index in calls.size():
		var mark: int = capture.last() if capture != null else 0
		var entry: Dictionary = _call(root, calls[index], index)
		if capture != null:
			var logged: Dictionary = capture.read({"after": mark, "limit": MAX_CALL_LOGGED, "kinds": LOGGED_KINDS})
			entry["logged"] = logged["entries"]
			entry["logged_complete"] = not bool(logged["truncated"]) and int(logged["skipped"]) == 0
		else:
			entry["logged"] = []
			entry["logged_complete"] = false
		answered.append(entry)
	var after: String = handlers.edit_digest(root)
	if after != before:
		return _fail("internal", "a reading call changed %s as the editor holds it (its semantic digest %s became %s), and no undo history records the change; no value is answered" % [root.scene_file_path, before, after])
	return _ok({
		"scene": root.scene_file_path,
		"digest": after,
		"unsaved": handlers.unsaved(root),
		"log_capture": capture != null,
		"calls": answered,
	})


static func _call(root: Node, call, index: int) -> Dictionary:
	var entry := {"index": index}
	if typeof(call) != TYPE_DICTIONARY:
		return _unread(entry, "invalid_input", "a call is an object")
	var method := str(call.get("method", ""))
	entry["method"] = method
	var spelled := str(call.get("node", "."))
	if spelled.begins_with("/") or spelled.split("/").has(".."):
		return _unread(entry, "invalid_input", "`%s` is not a node path relative to the scene root" % spelled)
	var receiver: Object = root if spelled == "." or spelled == "" else root.get_node_or_null(NodePath(spelled))
	if receiver == null:
		return _unread(entry, "not_found", "the scene has no node %s" % spelled)
	if call.get("resource") != null:
		var selected: Dictionary = _selected(receiver, call.get("resource"))
		if not selected["ok"]:
			return _unread(entry, selected["reason"], selected["detail"])
		receiver = selected["object"]
	var listed := _reading_class(receiver, method)
	if listed == "":
		return _unread(entry, "invalid_input", "%s is not a reading method of a %s" % [method, receiver.get_class()])
	var info: Dictionary = EditingMethods.method_info(receiver, method)
	if info.is_empty():
		return _unread(entry, "unsupported", "this Godot build has no %s.%s" % [listed, method])
	var given = call.get("arguments", [])
	if typeof(given) != TYPE_ARRAY or given.size() > EditingMethods.MAX_CALL_ARGUMENTS:
		return _unread(entry, "invalid_input", "a call takes at most %d arguments" % EditingMethods.MAX_CALL_ARGUMENTS)
	var arguments: Array = []
	for argument in given:
		var decoded: Dictionary = VariantCodec.decode(argument)
		if not decoded["ok"]:
			return _unread(entry, "invalid_input", decoded["reason"])
		arguments.append(decoded["value"])
	var problem := EditingMethods.signature_problem(receiver, method, info, arguments)
	if problem != "":
		return _unread(entry, "invalid_input", problem)
	if receiver is Gradient:
		receiver = receiver.duplicate()
	var value = receiver.callv(method, arguments)
	if typeof(value) == TYPE_OBJECT:
		return _unread(entry, "internal", "%s returned an object, which a read does not answer" % method)
	var literal := var_to_str(value)
	var size := literal.to_utf8_buffer().size()
	if size > MAX_READ_VALUE_BYTES:
		entry["value_bytes"] = size
		return _unread(entry, "too_large", "the value takes %d bytes of text, more than the %d a call answers" % [size, MAX_READ_VALUE_BYTES])
	entry["literal"] = literal
	return entry


## The table's class `object` belongs to for the reading `method`, or "".
static func _reading_class(object: Object, method: String) -> String:
	for listed in READING_METHODS:
		if ClassDB.is_parent_class(object.get_class(), listed) and READING_METHODS[listed].has(method):
			return listed
	return ""


## The resource of `base` a call's `resource` selector names: a property or a
## property path through the resources `base` holds, embedded or saved in
## their own files, since a read changes none of them, an animation of an
## AnimationMixer, or an animation library of one.
static func _selected(base: Object, selector) -> Dictionary:
	if typeof(selector) != TYPE_DICTIONARY or selector.size() != 1:
		return _refusal("invalid_input", "`resource` names one of property, animation, library")
	var kind := str(selector.keys()[0])
	var name := str(selector[kind])
	match kind:
		"property":
			var holder: Object = base
			for part in name.split(":"):
				if part == "" or not EditingMethods.has_property(holder, part):
					return _refusal("not_found", "%s names no property of a %s" % [name, holder.get_class()])
				var value = holder.get(part)
				if not (value is Resource) or value is Script:
					return _refusal("invalid_input", "%s holds no resource a read can call" % name)
				holder = value
			return {"ok": true, "object": holder}
		"animation":
			if not (base is AnimationMixer) or not base.has_animation(name):
				return _refusal("not_found", "no animation %s" % name)
			return {"ok": true, "object": base.get_animation(name)}
		"library":
			if not (base is AnimationMixer) or not base.has_animation_library(name):
				return _refusal("not_found", "no animation library %s" % name)
			return {"ok": true, "object": base.get_animation_library(name)}
	return _refusal("invalid_input", "`resource` names one of property, animation, library")
