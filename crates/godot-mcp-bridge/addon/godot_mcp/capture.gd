@tool
extends RefCounted
## `editor.capture.viewport` and `editor.capture.chunk`: what the editor
## shows, captured as PNG bytes that the server reads back in bounded chunks
## and checks against the length and SHA-256 reported here.
##
## A capture is the editor's whole window, or the 2D or one 3D viewport of the
## main screen shown. The plugin keeps the last few captures in memory only;
## nothing is written to the project or the editor's settings.

const MAX_CAPTURES := 4
const MAX_CHUNK_BYTES := 1048576
const MAX_WIDTH := 4096
## How many frames a capture waits for the editor to draw, at most. A
## low-processor editor draws only when something changed, so the capture asks
## for a redraw first; if no frame is drawn it reads what was drawn last.
const DRAW_FRAMES := 120

var _captures: Dictionary = {}
var _order: Array = []
var _next := 1


static func _fail(kind: String, message: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": "not_started"}


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "not_started"}


## Whether the viewport `target` is on screen: its container is shown.
static func _shown(target) -> bool:
	return target != null and target.get_parent() is Control and target.get_parent().is_visible_in_tree()


func viewport(payload: Dictionary) -> Dictionary:
	if DisplayServer.get_name() == "headless":
		return _fail("unavailable", "this editor runs headless and draws nothing; capture an editor that has a window")
	var view := str(payload.get("view", "window"))
	var target: Viewport = null
	match view:
		"window":
			target = EditorInterface.get_base_control().get_viewport()
		"2d":
			target = EditorInterface.get_editor_viewport_2d()
			if not _shown(target):
				return _fail("unavailable", "the 2D main screen is not shown; show it with editor.main_screen.set first")
		"3d":
			var index := int(payload.get("viewport", 0))
			if index < 0 or index > 3:
				return _fail("invalid_input", "the 3D main screen has viewports 0 to 3")
			target = EditorInterface.get_editor_viewport_3d(index)
			if not _shown(target):
				return _fail("unavailable", "3D viewport %d is not shown; show the 3D main screen with editor.main_screen.set, in a layout with that viewport" % index)
		_:
			return _fail("invalid_input", "`view` is window, 2d or 3d")
	await _drawn()
	var image: Image = target.get_texture().get_image()
	if image == null or image.is_empty():
		return _fail("unavailable", "the %s view produced no image" % view)
	var max_width: int = clampi(int(payload.get("max_width", 0)), 0, MAX_WIDTH)
	if max_width > 0 and image.get_width() > max_width:
		var height: int = maxi(1, int(round(float(image.get_height()) * max_width / image.get_width())))
		image.resize(max_width, height, Image.INTERPOLATE_BILINEAR)
	var png: PackedByteArray = image.save_png_to_buffer()
	return _ok(_keep(png, image.get_width(), image.get_height(), view))


## Wait for the editor to draw a frame, after asking it to.
func _drawn() -> void:
	var drawn := [false]
	var mark := func() -> void: drawn[0] = true
	RenderingServer.frame_post_draw.connect(mark, CONNECT_ONE_SHOT)
	var base: Control = EditorInterface.get_base_control()
	base.queue_redraw()
	var tree: SceneTree = base.get_tree()
	for frame in DRAW_FRAMES:
		if drawn[0]:
			break
		await tree.process_frame
	if RenderingServer.frame_post_draw.is_connected(mark):
		RenderingServer.frame_post_draw.disconnect(mark)


func _keep(png: PackedByteArray, width: int, height: int, view: String) -> Dictionary:
	var id := "capture-%d" % _next
	_next += 1
	_captures[id] = png
	_order.append(id)
	while _order.size() > MAX_CAPTURES:
		_captures.erase(_order.pop_front())
	var hashing := HashingContext.new()
	hashing.start(HashingContext.HASH_SHA256)
	if not png.is_empty():
		hashing.update(png)
	return {
		"capture_id": id,
		"width": width,
		"height": height,
		"view": view,
		"frames": [{"byte_length": png.size(), "sha256": hashing.finish().hex_encode()}],
	}


func chunk(payload: Dictionary) -> Dictionary:
	var id := str(payload.get("capture_id", ""))
	if not _captures.has(id):
		return _fail("not_found", "%s is not a capture this editor still holds" % id)
	if int(payload.get("frame", 0)) != 0:
		return _fail("invalid_input", "an editor capture has one frame")
	var bytes: PackedByteArray = _captures[id]
	var offset: int = int(payload.get("offset", 0))
	var length: int = clampi(int(payload.get("length", MAX_CHUNK_BYTES)), 1, MAX_CHUNK_BYTES)
	if offset < 0 or offset > bytes.size():
		return _fail("invalid_input", "offset %d is outside the capture's %d bytes" % [offset, bytes.size()])
	var slice: PackedByteArray = bytes.slice(offset, offset + length)
	return _ok({
		"capture_id": id,
		"frame": 0,
		"offset": offset,
		"length": slice.size(),
		"total": bytes.size(),
		"data_base64": Marshalls.raw_to_base64(slice),
	})
