@tool
extends RefCounted
## `editor.camera.read` and `editor.camera.set`: what the editor's 2D view and
## its four 3D viewports show, and moving the camera of one 3D viewport to look
## from one point at another.
##
## The 2D view is read from the 2D editor viewport's canvas transform. It is
## never moved: the 2D editor writes that transform from its own state each
## time it updates the view, so a plugin's value lasts only until then
## (measured on Godot 4.4.1 to 4.7.1).
##
## A 3D viewport is read through the camera the editor draws it with. Moving
## that camera holds from Godot 4.5.1 on, where the editor takes the camera's
## new place as its own (measured: still in place 120 frames later and after
## switching screens, on 4.5.1, 4.6.1 and 4.7.1). Godot 4.4.1 keeps its orbit
## pivot where the camera was and glides the camera to 4.0 units behind the
## point asked for, so it moves no camera. The editor drops a move made while
## the viewport is hidden once it shows it again, so only a shown viewport is
## moved. Every move is read back before it is answered.

const VariantCodec = preload("variant_codec.gd")

const VIEWPORTS := 4
## Frames the editor draws before a moved camera is read back.
const SETTLE_FRAMES := 5
const PROJECTIONS := ["perspective", "orthogonal", "frustum"]


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func _godot() -> String:
	var version: Dictionary = Engine.get_version_info()
	return "%d.%d.%d" % [int(version.get("major", 0)), int(version.get("minor", 0)), int(version.get("patch", 0))]


## Whether this editor keeps a 3D camera a plugin moves: Godot 4.5 and later.
static func can_set() -> bool:
	var version: Dictionary = Engine.get_version_info()
	return int(version.get("major", 0)) > 4 or (int(version.get("major", 0)) == 4 and int(version.get("minor", 0)) >= 5)


static func _shown(viewport) -> bool:
	return viewport != null and viewport.get_parent() is Control and viewport.get_parent().is_visible_in_tree()


static func _point(vector: Vector3) -> Dictionary:
	return {"x": vector.x, "y": vector.y, "z": vector.z}


static func _vector(value):
	if typeof(value) != TYPE_DICTIONARY or value.size() != 3:
		return null
	for axis in ["x", "y", "z"]:
		var coordinate = value.get(axis)
		if not (coordinate is float or coordinate is int) or not is_finite(float(coordinate)):
			return null
	return Vector3(float(value["x"]), float(value["y"]), float(value["z"]))


static func _tagged(value):
	var encoded: Dictionary = VariantCodec.encode(value)
	return encoded["value"] if encoded.get("ok", false) else null


## `editor.camera.read`: the main screen shown, the 2D view (`view` "2d"), the
## 3D viewports (`view` "3d"), or both; `viewport` 0 to 3 reads one 3D
## viewport only.
static func read(payload: Dictionary, main_screen: String) -> Dictionary:
	var view := str(payload.get("view", ""))
	var only := int(payload.get("viewport", -1))
	if not ["", "2d", "3d"].has(view):
		return _fail("invalid_input", "`view` is 2d or 3d")
	if only < -1 or only >= VIEWPORTS:
		return _fail("invalid_input", "`viewport` is 0 to %d" % (VIEWPORTS - 1))
	var answer := {"main_screen": main_screen}
	if view != "3d":
		answer["2d"] = _view_2d()
	if view != "2d":
		var views: Array = []
		for index in VIEWPORTS:
			if only == -1 or only == index:
				views.append(_view_3d(index))
		answer["3d"] = views
	return {"ok": true, "payload": answer, "mutation_status": "not_started"}


static func _view_2d():
	var viewport = EditorInterface.get_editor_viewport_2d()
	if viewport == null:
		return null
	var transform: Transform2D = viewport.global_canvas_transform
	var size := Vector2(viewport.size)
	var center: Vector2 = transform.affine_inverse() * (size / 2.0)
	return {
		"shown": _shown(viewport),
		"size": {"width": viewport.size.x, "height": viewport.size.y},
		"zoom": transform.get_scale().x,
		"center": {"x": center.x, "y": center.y},
		"transform": _tagged(transform),
	}


static func _view_3d(index: int) -> Dictionary:
	var viewport = EditorInterface.get_editor_viewport_3d(index)
	var entry := {"viewport": index, "shown": false, "size": null, "camera": null}
	if viewport == null:
		return entry
	entry["shown"] = _shown(viewport)
	entry["size"] = {"width": viewport.size.x, "height": viewport.size.y}
	var camera = viewport.get_camera_3d()
	if camera != null:
		entry["camera"] = describe(camera)
	return entry


## One camera as a read answers it: its global transform, its position and
## the direction it looks in, its projection and the lens values of that
## projection.
static func describe(camera: Camera3D) -> Dictionary:
	var transform: Transform3D = camera.global_transform
	var projection: int = camera.projection
	return {
		"transform": _tagged(transform),
		"position": _point(transform.origin),
		"forward": _point(-transform.basis.z.normalized()),
		"projection": PROJECTIONS[projection] if projection >= 0 and projection < PROJECTIONS.size() else str(projection),
		"fov": camera.fov,
		"size": camera.size,
		"near": camera.near,
		"far": camera.far,
	}


## `editor.camera.set`: place the camera of 3D viewport `viewport` at
## `position`, looking at `target` with no roll, and answer once a read-back
## SETTLE_FRAMES frames later still shows it there.
static func move(payload: Dictionary) -> Dictionary:
	if not can_set():
		return _fail("unsupported", "this editor (Godot %s) does not keep a camera a plugin moves: its 3D editor puts its orbit pivot at the camera and draws the view 4.0 units behind the point asked for (measured on Godot 4.4.1); cameras a plugin moves hold from Godot 4.5.1 on. Nothing was changed." % _godot())
	var index := int(payload.get("viewport", 0))
	if index < 0 or index >= VIEWPORTS:
		return _fail("invalid_input", "`viewport` is 0 to %d" % (VIEWPORTS - 1))
	var position = _vector(payload.get("position"))
	var target = _vector(payload.get("target"))
	if position == null or target == null:
		return _fail("invalid_input", "`position` and `target` are points {x, y, z} of finite numbers")
	if position.distance_to(target) < 0.0001:
		return _fail("invalid_input", "`position` and `target` are the same point, so the camera would look nowhere")
	var viewport = EditorInterface.get_editor_viewport_3d(index)
	var camera = viewport.get_camera_3d() if viewport != null else null
	if camera == null:
		return _fail("unavailable", "3D viewport %d has no camera in this editor" % index)
	var edited: Node = EditorInterface.get_edited_scene_root()
	if edited != null and (camera == edited or edited.is_ancestor_of(camera)):
		return _fail("conflict", "3D viewport %d shows a camera of the edited scene (%s), which only an edit of the scene moves; nothing was changed" % [index, str(edited.get_path_to(camera))])
	if not _shown(viewport):
		return _fail("conflict", "3D viewport %d is not shown, and the editor drops a camera moved while its viewport is hidden as soon as it shows it again (measured on Godot 4.5.1 to 4.7.1). Show the 3D main screen with editor.main_screen.set, in a layout that shows viewport %d, then move its camera. Nothing was changed." % [index, index])
	var direction: Vector3 = (target - position).normalized()
	# Looking straight up or down, "up" is the direction the view's top faces.
	var up := Vector3.UP if absf(direction.dot(Vector3.UP)) < 0.999 else Vector3.FORWARD
	var wanted := Transform3D(Basis(), position).looking_at(target, up)
	camera.global_transform = wanted
	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	for frame in SETTLE_FRAMES:
		await tree.process_frame
	if not is_instance_valid(camera):
		return _fail("uncertain", "3D viewport %d's camera was moved, but it was freed before it could be read back" % index, "committed")
	var now: Transform3D = camera.global_transform
	var described := describe(camera)
	if not now.is_equal_approx(wanted):
		return _fail("uncertain", "3D viewport %d's camera was moved to %s looking at %s, but %d frames later it is at %s looking along %s: the editor moved it again" % [index, str(position), str(target), SETTLE_FRAMES, str(now.origin), str(-now.basis.z.normalized())], "committed")
	return _ok({
		"viewport": index,
		"shown": _shown(viewport),
		"camera": described,
		"action": "godot-mcp: editor.camera.set",
	})
