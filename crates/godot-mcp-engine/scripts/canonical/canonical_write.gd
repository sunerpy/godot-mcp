# godot-mcp bundled canonical-write helper, version 4.
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
#         --script res://godot_mcp_helpers/4/canonical_write.gd \
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
#   * The helper writes ONLY inside `res://godot_mcp_helpers/4/staging/`. It
#     never touches the caller's target path. Publishing the staged artifact
#     onto the target is done by the Rust side, under the Todo 2 lease, as a
#     compare-and-swap against the recorded preimage.
#   * The helper refuses binary resources (`.scn`/`.res`) outright.
#   * A `batch` is a JSON array of typed operations the ENGINE applies to the
#     loaded scene tree or resource: nodes, properties (as tagged values),
#     scripts, instances, signal connections, groups and sub-resources. Every
#     operation is validated against the engine's own class database; the first
#     one that cannot be applied refuses the whole batch and nothing is staged.
#   * A batch's `call_method` calls one editing method of the table below on a
#     node the scene owns, on a resource the scene embeds, or on the resource a
#     resource batch edits, after checking its arguments against the method's
#     engine signature, once or once per argument list of its `repeat`. A
#     resource saved in its own file is never a receiver: the edit would change
#     that file too. What each call returned is answered as `results`, a JSON
#     array in batch order.
#   * A bake, a call of a method BAKING lists, runs once the helper's tree is
#     running: the region of a copy of the scene, without scripts and added to
#     the tree, bakes into the scene's own embedded resource, never on a thread,
#     so the batch goes on only once the engine has finished baking.
#   * `apply_batches` applies one batch to each of several existing scenes
#     and resources in one run and stages each result, so one launch proof
#     covers every staged file. Each file is edited as it is on disk; none
#     sees what the run does to another. The first file that cannot be
#     applied refuses the whole run and is named in the answer.
#   * `read_calls` calls reading methods of the second table on nodes of a
#     scene, on a resource or a `.gdshader`, or on resources either holds, and
#     answers each value in Godot's text spelling. It stages nothing.
#   * A property may be a property path: names separated by ":" pass through
#     properties holding resources the edited file embeds, and the rest names a
#     property of the last one and components of its value (`position:x`). A
#     resource may take a stored property it does not list yet, as a state
#     machine's `states/Run/node` or a bus layout's `bus/2/name`, which it must
#     list once written.
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

const HELPER_VERSION := "4"
const STAGING_PREFIX := "res://godot_mcp_helpers/4/staging/"
const TEXT_EXTENSIONS := ["tscn", "tres"]
const BINARY_EXTENSIONS := ["scn", "res"]

const HANDLE_PREFIX := "nh1:"

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
const MAX_BATCH_TARGETS := 16

const EXIT_OK := 0
const EXIT_USAGE := 64
const EXIT_REFUSED := 65
const EXIT_FAILED := 70

# Node handle -> the node object it was bound to before the batch ran.
var _handles: Dictionary = {}
# The `res://` path of the file the batch edits: a resource it embeds has this
# path followed by `::`.
var _owner_path := ""
# What each `call_method` returned, in batch order.
var _results: Array = []


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

	# The engine bakes navigation only for nodes inside the running SceneTree,
	# whose root joins it after this returns: a batch that bakes runs once the
	# first frame flushes deferred calls, after the project's autoloads have
	# started, as they do in every headless run.
	if _bakes(request):
		_finish.call_deferred(request, response_path)
		return
	_finish(request, response_path)


func _finish(request: Dictionary, response_path: String) -> void:
	var response: Dictionary = _run(request)
	if str(response.get("status", "")) == "ok" and not _results.is_empty():
		response["results"] = JSON.stringify(_results)
	var code: int = int(response.get("exit_code", EXIT_FAILED))
	response.erase("exit_code")
	response["helper_version"] = HELPER_VERSION
	if not _write_pairs(response_path, response):
		printerr("canonical_write: could not write the response document")
		quit(EXIT_FAILED)
		return
	quit(code)


# Whether the request's batch, or the batch of one of its targets, calls a
# BAKING method.
func _bakes(request: Dictionary) -> bool:
	var batches: Array = [str(request.get("batch", "[]"))]
	var targets = JSON.parse_string(str(request.get("targets", "[]")))
	if typeof(targets) == TYPE_ARRAY:
		for entry in targets:
			if typeof(entry) == TYPE_DICTIONARY:
				batches.append(str(entry.get("batch", "[]")))
	for text in batches:
		var parsed = JSON.parse_string(text)
		if typeof(parsed) != TYPE_ARRAY:
			continue
		for op in parsed:
			if typeof(op) != TYPE_DICTIONARY or str(op.get("op", "")) != "call_method":
				continue
			for listed in BAKING:
				if EDITING_METHODS[listed].has(str(op.get("method", ""))):
					return true
	return false

func _run(request: Dictionary) -> Dictionary:
	var operation: String = str(request.get("operation", ""))
	var target: String = str(request.get("target", ""))
	var artifact: String = str(request.get("artifact", ""))
	if operation == "apply_batches":
		return _apply_batches(request)

	if not artifact.begins_with("godot_mcp_helpers/"):
		return {"status": "refused", "reason": "artifact_outside_staging", "exit_code": EXIT_REFUSED}
	var artifact_res: String = _res(artifact)
	if not artifact_res.begins_with(STAGING_PREFIX):
		return {"status": "refused", "reason": "artifact_outside_staging", "exit_code": EXIT_REFUSED}

	_owner_path = _res(target) if target != "" else ""
	_results = []
	if target != "":
		var extension: String = target.get_extension().to_lower()
		if BINARY_EXTENSIONS.has(extension):
			return {"status": "refused", "reason": "binary_resource", "exit_code": EXIT_REFUSED}
		if not TEXT_EXTENSIONS.has(extension) and not (operation == "read_calls" and extension == "gdshader"):
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
		"read_calls":
			return _read_calls(request, target)
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


# `apply_batches`: apply one batch to each of several existing scenes and
# resources, in order, and stage each result in its own artifact. The answer
# lists each staged file with its UID and what its calls returned. The first
# file that cannot be applied refuses the whole run and is named.
func _apply_batches(request: Dictionary) -> Dictionary:
	var targets = JSON.parse_string(str(request.get("targets", "")))
	if typeof(targets) != TYPE_ARRAY or targets.is_empty() or targets.size() > MAX_BATCH_TARGETS:
		return _refuse("targets_not_a_list", -1, "the targets must list 1 to %d files" % MAX_BATCH_TARGETS)
	var staged: Array = []
	for index in targets.size():
		var entry = targets[index]
		if typeof(entry) != TYPE_DICTIONARY:
			return _refuse("target_not_an_object", -1, "target %d is not an object" % index)
		var target: String = str(entry.get("target", ""))
		var artifact: String = str(entry.get("artifact", ""))
		var answer: Dictionary = _apply_target(entry, target, artifact)
		if str(answer.get("status", "")) != "ok":
			answer["target_index"] = str(index)
			answer["target"] = _res(target)
			return answer
		staged.append({"target": target, "artifact": artifact, "uid": answer["uid"], "results": _results})
	_results = []
	return {"status": "ok", "exit_code": EXIT_OK, "staged": JSON.stringify(staged)}


# One file of `apply_batches`, checked as `_run` checks a single target.
func _apply_target(entry: Dictionary, target: String, artifact: String) -> Dictionary:
	if not artifact.begins_with("godot_mcp_helpers/") or not _res(artifact).begins_with(STAGING_PREFIX):
		return {"status": "refused", "reason": "artifact_outside_staging", "exit_code": EXIT_REFUSED}
	var extension: String = target.get_extension().to_lower()
	if BINARY_EXTENSIONS.has(extension):
		return {"status": "refused", "reason": "binary_resource", "exit_code": EXIT_REFUSED}
	if not TEXT_EXTENSIONS.has(extension):
		return {"status": "refused", "reason": "not_a_text_resource", "exit_code": EXIT_REFUSED}
	_owner_path = _res(target)
	_results = []
	var single := {"batch": str(entry.get("batch", "[]"))}
	if entry.has("handles"):
		single["handles"] = str(entry["handles"])
	return _apply_batch(single, target, _res(artifact))


# --- reads -----------------------------------------------------------------


# `read_calls`: call reading methods of the table on nodes of a scene, or on
# the resource read, or on resources either holds, and answer each value in
# Godot's own text spelling. Nothing is changed or staged. A call that cannot
# be made says why, and the others are still made. Each call is bracketed on
# stderr by two marker lines, so an engine error it raises can be told apart
# from the next call's.
func _read_calls(request: Dictionary, target: String) -> Dictionary:
	var parsed = JSON.parse_string(str(request.get("batch", "[]")))
	if typeof(parsed) != TYPE_ARRAY or parsed.is_empty() or parsed.size() > MAX_READ_CALLS:
		return _refuse("invalid_calls", -1, "the calls are a JSON array of 1 to %d calls" % MAX_READ_CALLS)
	var loaded = ResourceLoader.load(_res(target), "", ResourceLoader.CACHE_MODE_IGNORE)
	if loaded == null:
		return {"status": "failed", "reason": "target_not_loadable", "exit_code": EXIT_FAILED}
	var root: Node = null
	if loaded is PackedScene:
		root = loaded.instantiate(PackedScene.GEN_EDIT_STATE_MAIN)
		if root == null:
			return {"status": "failed", "reason": "target_not_instantiable", "exit_code": EXIT_FAILED}
	for index in parsed.size():
		printerr("godot_mcp_read_call_begin %d" % index)
		_results.append(_read_call(root, loaded, parsed[index], index))
		printerr("godot_mcp_read_call_end %d" % index)
	if root != null:
		root.free()
	return {"status": "ok", "exit_code": EXIT_OK}


func _read_call(root: Node, loaded: Resource, call, index: int) -> Dictionary:
	var entry := {"index": index}
	if typeof(call) != TYPE_DICTIONARY:
		return _unread(entry, "invalid_input", "a call is an object")
	var method: String = str(call.get("method", ""))
	entry["method"] = method
	var receiver: Object = loaded
	if root != null:
		var node: Node = _node(root, call.get("node", "."))
		if node == null:
			return _unread(entry, "not_found", "the scene has no node %s" % str(call.get("node", ".")))
		receiver = node
	elif call.get("node") != null:
		return _unread(entry, "invalid_input", "a resource has no nodes")
	if call.get("resource") != null:
		var found: Dictionary = _read_selected(receiver, call.get("resource"))
		if not found["ok"]:
			return _unread(entry, found["reason"], found["detail"])
		receiver = found["object"]
	var listed: String = _reading_class(receiver, method)
	if listed == "":
		return _unread(entry, "invalid_input", "%s is not a reading method of a %s" % [method, receiver.get_class()])
	var info: Dictionary = {}
	for candidate in ClassDB.class_get_method_list(receiver.get_class()):
		if str(candidate["name"]) == method:
			info = candidate
			break
	if info.is_empty():
		return _unread(entry, "unsupported", "this Godot build has no %s.%s" % [listed, method])
	var given = call.get("arguments", [])
	if typeof(given) != TYPE_ARRAY or given.size() > MAX_CALL_ARGUMENTS:
		return _unread(entry, "invalid_input", "a call takes at most %d arguments" % MAX_CALL_ARGUMENTS)
	var arguments: Array = []
	for argument in given:
		var decoded: Dictionary = _decode(argument)
		if not decoded["ok"]:
			return _unread(entry, "invalid_input", decoded["reason"])
		arguments.append(decoded["value"])
	var problem: String = _signature_problem(receiver, method, info, arguments)
	if problem != "":
		return _unread(entry, "invalid_input", problem)
	var value = receiver.callv(method, arguments)
	if typeof(value) == TYPE_OBJECT:
		return _unread(entry, "internal", "%s returned an object, which a read does not answer" % method)
	var literal: String = var_to_str(value)
	var size: int = literal.to_utf8_buffer().size()
	if size > MAX_READ_VALUE_BYTES:
		entry["value_bytes"] = size
		return _unread(entry, "too_large", "the value takes %d bytes of text, more than the %d a call answers" % [size, MAX_READ_VALUE_BYTES])
	entry["literal"] = literal
	return entry


func _unread(entry: Dictionary, kind: String, detail: String) -> Dictionary:
	entry["error_kind"] = kind
	entry["detail"] = detail.replace("\n", " ")
	return entry


# The table's class `object` belongs to for the reading `method`, or "".
func _reading_class(object: Object, method: String) -> String:
	for listed in READING_METHODS:
		if ClassDB.is_parent_class(object.get_class(), listed) and READING_METHODS[listed].has(method):
			return listed
	return ""


# The resource of `base` a read's `resource` selector names: a property or a
# property path through the resources `base` holds, embedded or saved in their
# own files, since a read changes none of them, an animation of an
# AnimationMixer or an AnimationLibrary, or an animation library of an
# AnimationMixer.
func _read_selected(base: Object, selector) -> Dictionary:
	if typeof(selector) != TYPE_DICTIONARY or selector.size() != 1:
		return _selector_refusal("invalid_input", "`resource` names one of property, animation, library")
	var kind: String = str(selector.keys()[0])
	var name: String = str(selector[kind])
	match kind:
		"property":
			var holder: Object = base
			for part in name.split(":"):
				if part == "" or not _has_property(holder, part):
					return _selector_refusal("not_found", "%s names no property of a %s" % [name, holder.get_class()])
				var value = holder.get(part)
				if not (value is Resource) or value is Script:
					return _selector_refusal("invalid_input", "%s holds no resource a read can call" % name)
				holder = value
			return {"ok": true, "object": holder}
		"animation":
			if not (base is AnimationMixer or base is AnimationLibrary) or not base.has_animation(name):
				return _selector_refusal("not_found", "no animation %s" % name)
			return {"ok": true, "object": base.get_animation(name)}
		"library":
			if not (base is AnimationMixer) or not base.has_animation_library(name):
				return _selector_refusal("not_found", "no animation library %s" % name)
			return {"ok": true, "object": base.get_animation_library(name)}
	return _selector_refusal("invalid_input", "`resource` names one of property, animation, library")


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
				var made: Dictionary = _instantiate(str(op.get("type", "Node")), "Node")
				if not made["ok"]:
					return _refuse("unknown_node_type", index, made["detail"])
				child = made["object"]
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
		"call_method":
			var node: Node = _node(root, op.get("node", "."))
			if node == null or not _owned(root, node):
				return _refuse("node_not_found", index, str(op.get("node", ".")))
			var baking: String = _listed_class(node, str(op.get("method", "")))
			if op.get("resource") == null and BAKING.has(baking):
				return _bake(root, node, baking, op, index)
			return _call_method(node, op, index)
		_:
			return _refuse("unknown_operation", index, kind)
	return {"status": "ok"}


# A BAKING call: the region rebuilds the resource its BAKING property holds
# from the nodes the engine parses for it. The engine parses only a tree that
# is inside the SceneTree, so the region bakes as part of a copy of the whole
# scene added to it — groups kept, scripts and signal connections not, so
# none of the scene's scripts runs — into the very resource the scene's region
# holds. The call is always made with `on_thread` false, so it returns only
# once the bake is done, and what it produced is answered with the call.
func _bake(root: Node, node: Node, listed: String, op: Dictionary, index: int) -> Dictionary:
	var method: String = str(op.get("method", ""))
	if op.has("repeat"):
		return _refuse("invalid_arguments", index, "%s bakes once; `repeat` does not apply" % method)
	var given = op.get("arguments", [])
	if typeof(given) != TYPE_ARRAY or given.size() > 1:
		return _refuse("invalid_arguments", index, "%s takes no argument or on_thread false" % method)
	if given.size() == 1:
		var decoded: Dictionary = _decode(given[0])
		if not decoded["ok"] or typeof(decoded["value"]) != TYPE_BOOL or decoded["value"]:
			return _refuse("invalid_arguments", index, "%s bakes on the calling thread: on_thread must be false" % method)
	var property: String = BAKING[listed]
	var held = node.get(property)
	if not (held is Resource):
		return _refuse("not_a_resource", index, "the %s holds no %s to bake into; set one first" % [node.get_class(), property])
	if not get_root().is_inside_tree():
		return {"status": "failed", "reason": "tree_not_live", "op_index": str(index), "detail": "the helper's tree is not running, so the engine would bake nothing", "exit_code": EXIT_FAILED}
	if not _embedded(held):
		return _refuse("external_resource", index, "%s is the file %s, which a bake would change too" % [property, held.resource_path])
	var copy: Node = root.duplicate(Node.DUPLICATE_GROUPS)
	var twin: Node = copy.get_node(root.get_path_to(node))
	twin.set(property, held)
	get_root().add_child(copy)
	twin.call(method, false)
	get_root().remove_child(copy)
	copy.free()
	_results.append({"index": index, "method": method, "value": null, "baked": _baked(held)})
	return {"status": "ok"}


# What a bake produced in `held`.
func _baked(held: Resource) -> Dictionary:
	var baked := {
		"class": held.get_class(),
		"polygons": int(held.get_polygon_count()),
		"vertices": int(held.get_vertices().size()),
	}
	if held is NavigationPolygon:
		baked["outlines"] = int(held.get_outline_count())
	return baked


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
			"call_method":
				result = _call_method(resource, op, index)
			_:
				result = _refuse("unknown_operation", index, kind)
		if result["status"] != "ok":
			return result
	return {"status": "ok"}


# Whether `object` lists `property`, a dynamic property such as a state
# machine's `states/Run/node` included.
func _has_property(object: Object, property: String) -> bool:
	for entry in object.get_property_list():
		if str(entry["name"]) == property:
			return true
	return false


# Whether `resource` belongs to the edited file: unsaved, or one of the
# sub-resources that file embeds.
func _embedded(resource: Resource) -> bool:
	var path: String = resource.resource_path
	return path == "" or path.begins_with(_owner_path + "::")


# A new object of the class `name` inheriting `base`: an engine class, or a
# global script class (`class_name`) whose engine base inherits `base`, with
# its script attached. Answers {"ok": true, "object"} or {"ok": false, "detail"}.
func _instantiate(name: String, base: String) -> Dictionary:
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
	var native: String = String(script.get_instance_base_type())
	if not ClassDB.can_instantiate(native) or not (native == base or ClassDB.is_parent_class(native, base)):
		return {"ok": false, "detail": "%s extends %s, not %s" % [name, native, base]}
	var object: Object = ClassDB.instantiate(native)
	object.set_script(script)
	if object.get_script() != script:
		if not (object is RefCounted):
			object.free()
		return {"ok": false, "detail": "%s could not be given its script %s" % [name, path]}
	return {"ok": true, "object": object}


# Where the property path `path` leads from `base`: each name but the last
# that names a property holding a resource passes into it, which must be one
# the edited file embeds and not a script; with `to_resource` every name must.
# Answers {"ok": true, "holder", "leaf"}, `leaf` being what remains of the path
# on `holder`, or a refusal {"ok": false, "reason", "detail"}.
func _walk(base: Object, path: String, to_resource: bool = false) -> Dictionary:
	var parts: PackedStringArray = path.split(":")
	for part in parts:
		if part == "":
			return _selector_refusal("invalid_property_path", "%s has an empty name" % path)
	var hops: int = parts.size() if to_resource else parts.size() - 1
	var holder: Object = base
	var at: int = 0
	while at < hops:
		var passed: String = ":".join(parts.slice(0, at + 1))
		if not _has_property(holder, parts[at]):
			if to_resource:
				return _selector_refusal("unknown_property", passed)
			break
		var value = holder.get(parts[at])
		if not (value is Object):
			if to_resource:
				return _selector_refusal("not_a_resource", "%s holds no resource" % passed)
			break
		if not (value is Resource):
			return _selector_refusal("not_a_resource", "%s holds an object that is not a resource" % passed)
		if value is Script:
			return _selector_refusal("script_resource", "%s is a script; change scripts with script.patch" % passed)
		if not _embedded(value):
			return _selector_refusal("external_resource", "%s is the file %s; edit it with resource.batch.apply" % [passed, value.resource_path])
		holder = value
		at += 1
	return {"ok": true, "holder": holder, "leaf": ":".join(parts.slice(at))}


# Whether a value read back as `after` is a write of `value` that the property
# took: the same object, null, or a value of the same type, with either numeric
# type for a number and either string type for a string.
# Whether the engine kept exactly the value written, for the types whose values
# compare exactly. A setter may replace a value of the right type, as a player
# keeps `Master` for a bus the layout lacks; floats and the types built of them
# are compared by type only, since the engine stores them rounded.
func _kept(value, after) -> bool:
	match typeof(value):
		TYPE_BOOL, TYPE_STRING, TYPE_STRING_NAME:
			return str(after) == str(value)
		TYPE_INT:
			return (typeof(after) == TYPE_INT or typeof(after) == TYPE_FLOAT) and float(after) == float(value)
	return true


# A refusal of a write the engine took as another type, or replaced.
func _not_written(value, after, path: String, index: int) -> Dictionary:
	if not _took(value, after):
		return _refuse("value_type_mismatch", index, path + " is " + type_string(typeof(after)))
	if not _kept(value, after):
		return _refuse("value_not_kept", index, "%s kept %s, not %s" % [path, str(after), str(value)])
	return {}


func _took(value, after) -> bool:
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


func _set_property(object: Object, op: Dictionary, index: int, clear: bool) -> Dictionary:
	var path: String = str(op.get("property", ""))
	if path == "":
		return _refuse("unknown_property", index, path)
	var found: Dictionary = _walk(object, path)
	if not found["ok"]:
		return _refuse(found["reason"], index, found["detail"])
	var holder: Object = found["holder"]
	var leaf: String = found["leaf"]
	var value
	if clear:
		if leaf.contains(":"):
			return _refuse("invalid_property_path", index, "clear_property clears a whole property, not %s" % leaf)
		if not _has_property(holder, leaf):
			return _refuse("unknown_property", index, path)
		if holder.property_can_revert(leaf):
			value = holder.property_get_revert(leaf)
		else:
			value = ClassDB.class_get_property_default_value(holder.get_class(), leaf)
	else:
		var decoded: Dictionary = _decode(op.get("value"))
		if not decoded["ok"]:
			return _refuse("invalid_value", index, decoded["reason"])
		value = decoded["value"]
	return _write(found, path, value, index, not clear)


# Write `value` where a walk led. A property the holder lists is written with
# `set`, or `set_indexed` for a component of its value; a metadata entry is
# added; and a resource may take a stored property it does not list yet, which
# it must list once written.
func _write(found: Dictionary, path: String, value, index: int, check: bool) -> Dictionary:
	var holder: Object = found["holder"]
	var leaf: String = found["leaf"]
	var name: String = leaf.get_slice(":", 0)
	var component: bool = leaf.contains(":")
	var listed: bool = _has_property(holder, name)
	var metadata: bool = name.begins_with("metadata/") and not component
	if component:
		if not listed:
			return _refuse("unknown_property", index, "a %s has no property %s" % [holder.get_class(), name])
		if holder.get(name) == null:
			return _refuse("unknown_property", index, "%s holds nothing; set it first" % name)
		if holder.get_indexed(NodePath(leaf)) == null:
			return _refuse("unknown_property", index, "%s names no part of %s" % [leaf, name])
		holder.set_indexed(NodePath(leaf), value)
		if check:
			var refused: Dictionary = _not_written(value, holder.get_indexed(NodePath(leaf)), path, index)
			if not refused.is_empty():
				return refused
		return {"status": "ok"}
	if not listed and metadata and not name.trim_prefix("metadata/").is_valid_identifier():
		return _refuse("invalid_property_path", index, "%s is not a metadata name" % name.trim_prefix("metadata/"))
	if not listed and not metadata and not (holder is Resource):
		return _refuse("unknown_property", index, path)
	holder.set(leaf, value)
	if not _has_property(holder, leaf):
		return _refuse("unknown_property", index, path)
	if check:
		var refused: Dictionary = _not_written(value, holder.get(leaf), path, index)
		if not refused.is_empty():
			return refused
	return {"status": "ok"}


func _set_subresource(object: Object, op: Dictionary, index: int) -> Dictionary:
	var path: String = str(op.get("property", ""))
	if path == "":
		return _refuse("unknown_property", index, path)
	var found: Dictionary = _walk(object, path)
	if not found["ok"]:
		return _refuse(found["reason"], index, found["detail"])
	if str(found["leaf"]).contains(":"):
		return _refuse("invalid_property_path", index, "%s names part of a value, not a property" % path)
	var made: Dictionary = _instantiate(str(op.get("class", "")), "Resource")
	if not made["ok"]:
		return _refuse("unknown_resource_class", index, made["detail"])
	var resource: Resource = made["object"]
	var properties = op.get("properties", {})
	if typeof(properties) != TYPE_DICTIONARY:
		return _refuse("invalid_value", index, "properties must be an object")
	for key in properties.keys():
		if not _has_property(resource, str(key)):
			return _refuse("unknown_property", index, str(key))
		var decoded: Dictionary = _decode(properties[key])
		if not decoded["ok"]:
			return _refuse("invalid_value", index, decoded["reason"])
		resource.set(str(key), decoded["value"])
		var refused: Dictionary = _not_written(decoded["value"], resource.get(str(key)), str(key), index)
		if not refused.is_empty():
			return refused
	return _write(found, path, resource, index, true)


# --- editing calls -------------------------------------------------------------


# `call_method`: call a listed editing method on `base`, a node the scene owns
# or the resource a resource batch edits, or on the resource of `base` that
# the `resource` selector names, once or once per argument list of `repeat`.
func _call_method(base: Object, op: Dictionary, index: int) -> Dictionary:
	var receiver: Object = base
	if op.get("resource") != null:
		var found: Dictionary = _selected_resource(base, op.get("resource"))
		if not found["ok"]:
			return _refuse(found["reason"], index, found["detail"])
		receiver = found["object"]
	var method: String = str(op.get("method", ""))
	var listed: String = _listed_class(receiver, method)
	if listed == "":
		return _refuse("method_not_listed", index, "%s cannot be called on a %s" % [method, receiver.get_class()])
	var given: Array = []
	if op.has("repeat"):
		var repeat = op["repeat"]
		if op.has("arguments") or typeof(repeat) != TYPE_ARRAY or repeat.is_empty() or repeat.size() > MAX_REPEATED_CALLS:
			return _refuse("invalid_arguments", index, "`repeat` lists 1 to %d argument lists, without `arguments`" % MAX_REPEATED_CALLS)
		given = repeat
	else:
		given = [op.get("arguments", [])]
	var info: Dictionary = {}
	for candidate in ClassDB.class_get_method_list(receiver.get_class()):
		if str(candidate["name"]) == method:
			info = candidate
			break
	var lists: Array = []
	for list in given:
		if typeof(list) != TYPE_ARRAY or list.size() > MAX_CALL_ARGUMENTS:
			return _refuse("invalid_arguments", index, "a call takes at most %d arguments" % MAX_CALL_ARGUMENTS)
		var arguments: Array = []
		for argument in list:
			var decoded: Dictionary = _decode_argument(argument)
			if not decoded["ok"]:
				return _refuse("invalid_value", index, decoded["reason"])
			arguments.append(decoded["value"])
		var problem: String = _signature_problem(receiver, method, info, arguments)
		if problem != "":
			return _refuse("invalid_arguments", index, problem)
		lists.append(arguments)
	var values: Array = []
	for arguments in lists:
		values.append(receiver.callv(method, arguments))
	# A class in SETTLE brings its stored properties up to date, so the staged
	# bytes hold no entry the receiver no longer shows.
	if SETTLE.has(listed):
		receiver.call(SETTLE[listed])
	if op.has("repeat"):
		var encoded: Array = []
		for value in values:
			encoded.append(_encode_result(value))
		_results.append({"index": index, "method": method, "values": encoded})
	else:
		_results.append({"index": index, "method": method, "value": _encode_result(values[0])})
	return {"status": "ok"}


# The table's class `object` belongs to for `method`, or "" when none lists it.
func _listed_class(object: Object, method: String) -> String:
	for listed in EDITING_METHODS:
		if ClassDB.is_parent_class(object.get_class(), listed) and EDITING_METHODS[listed].has(method):
			return listed
	return ""


func _selector_refusal(reason: String, detail: String) -> Dictionary:
	return {"ok": false, "reason": reason, "detail": detail}


# The resource of `base` that `selector` names — by a property or a property
# path through embedded resources, or as an animation or an animation library
# of an AnimationMixer — when the edited file embeds it.
func _selected_resource(base: Object, selector) -> Dictionary:
	if typeof(selector) != TYPE_DICTIONARY or selector.size() != 1:
		return _selector_refusal("invalid_selector", "`resource` names one of property, animation, library")
	var kind: String = str(selector.keys()[0])
	var name: String = str(selector[kind])
	var resource = null
	match kind:
		"property":
			var found: Dictionary = _walk(base, name, true)
			if not found["ok"]:
				return found
			return {"ok": true, "object": found["holder"]}
		"animation":
			if not (base is AnimationMixer) or not base.has_animation(name):
				return _selector_refusal("unknown_animation", name)
			resource = base.get_animation(name)
		"library":
			if not (base is AnimationMixer) or not base.has_animation_library(name):
				return _selector_refusal("unknown_animation_library", name)
			resource = base.get_animation_library(name)
		_:
			return _selector_refusal("invalid_selector", "`resource` names one of property, animation, library")
	if not (resource is Resource):
		return _selector_refusal("not_a_resource", "%s %s holds no resource" % [kind, name])
	if not _embedded(resource):
		return _selector_refusal("external_resource", "%s is the file %s; edit it with resource.batch.apply" % [name, resource.resource_path])
	return {"ok": true, "object": resource}


# One argument: a new embedded resource with optional properties, of any
# class but a script or a GDExtension, or a tagged value.
func _decode_argument(tagged) -> Dictionary:
	if typeof(tagged) == TYPE_DICTIONARY and str(tagged.get("type", "")) == "new_resource":
		var value = tagged.get("value")
		if tagged.size() != 2 or typeof(value) != TYPE_DICTIONARY or not value.has("class") \
				or value.size() > (2 if value.has("properties") else 1):
			return _bad("a new resource is {type: new_resource, value: {class, properties?}}")
		var resource_class: String = str(value["class"])
		for forbidden in NEW_RESOURCE_FORBIDDEN:
			if resource_class == forbidden or ClassDB.class_exists(resource_class) and ClassDB.is_parent_class(resource_class, forbidden):
				return _bad("a new resource cannot be a %s" % forbidden)
		var made: Dictionary = _instantiate(resource_class, "Resource")
		if not made["ok"]:
			return _bad(made["detail"])
		var resource: Resource = made["object"]
		var properties = value.get("properties", {})
		if typeof(properties) != TYPE_DICTIONARY:
			return _bad("a new resource's properties are an object")
		for key in properties:
			var property: String = str(key)
			if not _has_property(resource, property):
				return _bad("a %s has no property %s" % [resource_class, property])
			var decoded: Dictionary = _decode(properties[key])
			if not decoded["ok"]:
				return decoded
			resource.set(property, decoded["value"])
			if not _took(decoded["value"], resource.get(property)):
				return _bad("%s of a %s is a %s" % [property, resource_class, type_string(typeof(resource.get(property)))])
			if not _kept(decoded["value"], resource.get(property)):
				return _bad("%s of a %s kept %s, not %s" % [property, resource_class, str(resource.get(property)), str(decoded["value"])])
		return _good(resource)
	return _decode(tagged)


# Why `arguments` do not fit `method`'s engine signature `info` on `object`, or "".
func _signature_problem(object: Object, method: String, info: Dictionary, arguments: Array) -> String:
	if info.is_empty():
		return "%s has no method %s" % [object.get_class(), method]
	var parameters: Array = info.get("args", [])
	var defaults: Array = info.get("default_args", [])
	var least: int = parameters.size() - defaults.size()
	if arguments.size() < least or arguments.size() > parameters.size():
		return "%s.%s takes %d to %d arguments, not %d" % [object.get_class(), method, least, parameters.size(), arguments.size()]
	for position in arguments.size():
		var expected: int = int(parameters[position].get("type", TYPE_NIL))
		var argument = arguments[position]
		var actual: int = typeof(argument)
		if expected == TYPE_NIL or actual == expected and expected != TYPE_OBJECT:
			continue
		if expected == TYPE_OBJECT:
			var wanted: String = str(parameters[position].get("class_name", ""))
			if actual == TYPE_NIL or actual == TYPE_OBJECT and (wanted == "" or argument.is_class(wanted)):
				continue
			return "argument %d of %s must be a %s" % [position, method, wanted if wanted != "" else "object"]
		if expected == TYPE_FLOAT and actual == TYPE_INT:
			continue
		if expected == TYPE_INT and actual == TYPE_FLOAT and float(argument) == floor(float(argument)):
			continue
		if [TYPE_STRING, TYPE_STRING_NAME].has(expected) and [TYPE_STRING, TYPE_STRING_NAME].has(actual):
			continue
		if expected == TYPE_NODE_PATH and [TYPE_STRING, TYPE_STRING_NAME].has(actual):
			continue
		return "argument %d of %s must be %s, not %s" % [position, method, type_string(expected), type_string(actual)]
	return ""


# The tagged spelling of a call's result, null when it returned nothing. Every
# listed method returns nothing or an integer.
func _encode_result(value):
	match typeof(value):
		TYPE_NIL:
			return null
		TYPE_BOOL:
			return {"type": "bool", "value": value}
		TYPE_INT:
			return {"type": "integer", "value": value}
		TYPE_FLOAT:
			if is_finite(value):
				return {"type": "float", "value": value}
	return {"type": "unrepresentable", "value": {"variant_type": type_string(typeof(value))}}


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
