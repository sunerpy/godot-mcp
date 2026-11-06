extends SceneTree
## Image steps for the documentation site's pictures, run by a headless Godot inside an empty
## scratch project (scripts/site/capture_screens.py copies this file into one):
##
##   godot --headless --path <scratch> --script res://image_tool.gd -- webp <in.png> <out.webp> [...]
##   godot --headless --path <scratch> --script res://image_tool.gd -- fit <in.png> <out.png> <width> <height>
##
## `webp` writes each pair as lossless WebP, so an editor's small text stays sharp. `fit` crops an
## image with transparency to the pixels whose alpha is not zero, then scales it with Lanczos to fit
## inside width x height, keeping its aspect ratio; the demo project's art is made this way
## (examples/site-demo/art/README.md). Any failure prints why on stderr and exits 1.


func _init() -> void:
	quit(_run(OS.get_cmdline_user_args()))


func _run(args: PackedStringArray) -> int:
	if args.is_empty():
		return _fail("usage: webp <in.png> <out.webp> [...] | fit <in.png> <out.png> <width> <height>")
	match args[0]:
		"webp":
			return _webp(args.slice(1))
		"fit":
			return _fit(args.slice(1))
	return _fail("unknown command %s" % args[0])


func _webp(pairs: PackedStringArray) -> int:
	if pairs.is_empty() or pairs.size() % 2 != 0:
		return _fail("webp needs pairs of <in.png> <out.webp>")
	for i in range(0, pairs.size(), 2):
		var image := Image.load_from_file(pairs[i])
		if image == null or image.is_empty():
			return _fail("cannot read %s" % pairs[i])
		var saved := image.save_webp(pairs[i + 1], false)
		if saved != OK:
			return _fail("cannot write %s: %s" % [pairs[i + 1], error_string(saved)])
		print("webp %s %dx%d" % [pairs[i + 1], image.get_width(), image.get_height()])
	return 0


func _fit(args: PackedStringArray) -> int:
	if args.size() != 4:
		return _fail("fit needs <in.png> <out.png> <width> <height>")
	var image := Image.load_from_file(args[0])
	if image == null or image.is_empty():
		return _fail("cannot read %s" % args[0])
	var box := Vector2i(args[2].to_int(), args[3].to_int())
	if box.x <= 0 or box.y <= 0:
		return _fail("width and height must be positive")
	if image.detect_alpha() != Image.ALPHA_NONE:
		var used := image.get_used_rect()
		if used.size.x <= 0 or used.size.y <= 0:
			return _fail("%s has no visible pixel" % args[0])
		image = image.get_region(used)
	var scale := minf(float(box.x) / image.get_width(), float(box.y) / image.get_height())
	var size := Vector2i(maxi(1, roundi(image.get_width() * scale)), maxi(1, roundi(image.get_height() * scale)))
	image.resize(size.x, size.y, Image.INTERPOLATE_LANCZOS)
	var saved := image.save_png(args[1])
	if saved != OK:
		return _fail("cannot write %s: %s" % [args[1], error_string(saved)])
	print("fit %s %dx%d" % [args[1], size.x, size.y])
	return 0


func _fail(message: String) -> int:
	printerr("image_tool: %s" % message)
	return 1
