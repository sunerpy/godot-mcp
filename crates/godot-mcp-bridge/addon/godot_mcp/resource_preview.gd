@tool
extends RefCounted
## `editor.preview.resource`: the preview the editor makes of one resource,
## and its small thumbnail, kept as a capture the server reads back in chunks
## (`capture.gd`).
##
## The editor makes previews with its renderer. A headless editor has none and
## never answers a preview request: on Godot 4.4.1 to 4.7.1 no answer came in
## 600 frames for textures, SVG images, materials, meshes, gradients, scenes,
## scripts or sounds (measured). A windowed editor previews textures, SVG
## images, materials, meshes, gradients and scripts (measured under Xvfb with
## the OpenGL renderer); it gives a scene a preview only once it saved that
## scene with a thumbnail, and a mesh no thumbnail. When no preview comes, or
## the editor answers one that is empty or a single color (what it shows when
## it has nothing to draw), the answer is `unavailable`, never an image.

const DEFAULT_TIMEOUT_MS := 10000
const MIN_TIMEOUT_MS := 100
const MAX_TIMEOUT_MS := 60000
## How many pixels per side the single-color check samples, at most.
const SAMPLES := 16

var _received: Dictionary = {}
var _next := 1


static func _fail(kind: String, message: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": "not_started"}


func _on_preview(_path: String, preview, thumbnail, userdata) -> void:
	_received[str(userdata)] = {"preview": preview, "thumbnail": thumbnail}


## The image of `texture` as PNG can store it, or null when there is none or
## it is empty or a single color.
static func _image(texture):
	if not (texture is Texture2D):
		return null
	var image: Image = texture.get_image()
	if image == null or image.is_empty():
		return null
	if image.is_compressed():
		image = image.duplicate()
		if image.decompress() != OK:
			return null
	var first := image.get_pixel(0, 0)
	var step_x := maxi(1, image.get_width() / SAMPLES)
	var step_y := maxi(1, image.get_height() / SAMPLES)
	for y in range(0, image.get_height(), step_y):
		for x in range(0, image.get_width(), step_x):
			if not image.get_pixel(x, y).is_equal_approx(first):
				return image
	return null


func preview(payload: Dictionary, captures) -> Dictionary:
	var path := str(payload.get("path", ""))
	var timeout_ms := int(payload.get("timeout_ms", DEFAULT_TIMEOUT_MS))
	if not path.begins_with("res://") or path.contains("::") or path.get_file() == "":
		return _fail("invalid_input", "`path` must name a res:// file of the project")
	if timeout_ms < MIN_TIMEOUT_MS or timeout_ms > MAX_TIMEOUT_MS:
		return _fail("invalid_input", "`timeout_ms` is %d to %d" % [MIN_TIMEOUT_MS, MAX_TIMEOUT_MS])
	if not FileAccess.file_exists(path):
		return _fail("not_found", "%s does not exist" % path)
	if DisplayServer.get_name() == "headless":
		return _fail("unavailable", "this editor runs without a display, and the editor makes resource previews with its renderer: a headless editor never answers a preview request (measured on Godot 4.4.1 to 4.7.1). Preview %s in an editor with a window." % path)
	var key := "preview-%d" % _next
	_next += 1
	var tree: SceneTree = EditorInterface.get_base_control().get_tree()
	EditorInterface.get_resource_previewer().queue_resource_preview(path, self, "_on_preview", key)
	var deadline := Time.get_ticks_msec() + timeout_ms
	while not _received.has(key) and Time.get_ticks_msec() < deadline:
		await tree.process_frame
	if not _received.has(key):
		return _fail("unavailable", "the editor made no preview of %s within %d ms" % [path, timeout_ms])
	var answered: Dictionary = _received[key]
	_received.erase(key)
	var image = _image(answered["preview"])
	if image == null:
		var why := "no image" if not (answered["preview"] is Texture2D) else "an empty or single-color image"
		return _fail("unavailable", "the editor answered %s as the preview of %s, so it has no preview to show for it (a scene gets one only once the editor saved it with a thumbnail)" % [why, path])
	var pngs: Array = [image.save_png_to_buffer()]
	var images: Array = [{"role": "preview", "width": image.get_width(), "height": image.get_height()}]
	var thumbnail = _image(answered["thumbnail"])
	if thumbnail != null:
		pngs.append(thumbnail.save_png_to_buffer())
		images.append({"role": "thumbnail", "width": thumbnail.get_width(), "height": thumbnail.get_height()})
	for png in pngs:
		if png.is_empty():
			return _fail("unavailable", "the preview of %s could not be stored as PNG" % path)
	return {
		"ok": true,
		"payload": captures.keep(pngs, {"path": path, "images": images}),
		"mutation_status": "not_started",
	}
