## The ADR 0010 benchmark: the runtime probe's tree read over a 10,000-node
## scene, in GDScript exactly as `runtime_probe.gd` does it and natively
## through the spike's `NativeTreeSnapshot`, on the same nodes.
##
## Run headless with the spike's extension loaded:
##   godot --headless --path <bench project> --script res://bench.gd
## It prints one `BENCH {json}` line and exits 0 only when both
## implementations produced byte-identical snapshots.
extends SceneTree

const BRANCHES := 100
const LEAVES := 99
const LIMIT := 10000
const DEPTH := 64
const RUNS := 10


func _initialize() -> void:
	var root := Node2D.new()
	root.name = "BenchRoot"
	for branch in BRANCHES:
		var group := Node2D.new()
		group.name = "Branch%d" % branch
		group.add_to_group("branches")
		root.add_child(group)
		for leaf in LEAVES:
			var node: Node = Node2D.new() if leaf % 2 == 0 else Node.new()
			node.name = "Leaf%d" % leaf
			if leaf % 10 == 0:
				node.add_to_group("tens")
			group.add_child(node)
	get_root().add_child(root)
	_run.call_deferred(root)


# Copied verbatim from runtime_probe.gd's _describe_node.
func _describe_node(node: Node) -> Dictionary:
	var script = node.get_script()
	var entry := {
		"path": str(node.get_path()),
		"name": str(node.name),
		"class": node.get_class(),
		"script": script.resource_path if script != null else null,
		"children": node.get_child_count(),
		"groups": [],
	}
	for group in node.get_groups():
		if not str(group).begins_with("_"):
			entry["groups"].append(str(group))
	if node is CanvasItem:
		entry["visible"] = (node as CanvasItem).is_visible_in_tree()
	elif node is Node3D:
		entry["visible"] = (node as Node3D).is_visible_in_tree()
	return entry


# The walk runtime_probe.gd's _tree shipped with before this spike measured it:
# pop_front() shifts the whole pending array on every step.
func _gd_tree_pop_front(start: Node, depth: int, limit: int) -> Array:
	var nodes: Array = []
	var pending: Array = [[start, 0]]
	while not pending.is_empty():
		var item: Array = pending.pop_front()
		var node: Node = item[0]
		if nodes.size() >= limit:
			break
		nodes.append(_describe_node(node))
		if int(item[1]) < depth:
			for child in node.get_children():
				pending.append([child, int(item[1]) + 1])
	return nodes


# runtime_probe.gd's _tree walk as it ships now, without the probe's own-node
# filter and the answer envelope.
func _gd_tree(start: Node, depth: int, limit: int) -> Array:
	var nodes: Array = []
	var pending: Array = [[start, 0]]
	var cursor := 0
	while cursor < pending.size():
		var item: Array = pending[cursor]
		cursor += 1
		var node: Node = item[0]
		if nodes.size() >= limit:
			break
		nodes.append(_describe_node(node))
		if int(item[1]) < depth:
			for child in node.get_children():
				pending.append([child, int(item[1]) + 1])
	return nodes


func _time(work: Callable) -> Dictionary:
	var times: Array = []
	var result = null
	for run in RUNS + 1:
		var started := Time.get_ticks_usec()
		result = work.call()
		var elapsed := Time.get_ticks_usec() - started
		if run > 0:
			times.append(elapsed)
	times.sort()
	return {
		"result": result,
		"us": {"min": times[0], "median": times[times.size() / 2], "max": times[-1]},
	}


func _run(root: Node) -> void:
	if not ClassDB.class_exists("NativeTreeSnapshot"):
		print("BENCH " + JSON.stringify({"error": "the spike extension did not load"}))
		quit(2)
		return
	var native_snapshot = ClassDB.instantiate("NativeTreeSnapshot")
	var scripted := _time(func(): return _gd_tree(root, DEPTH, LIMIT))
	var native := _time(func(): return native_snapshot.describe_tree(root, DEPTH, LIMIT))
	var before_fix := _time(func(): return _gd_tree_pop_front(root, DEPTH, LIMIT))
	var scripted_text := JSON.stringify(scripted["result"])
	var native_text := JSON.stringify(native["result"])

	var before := OS.get_static_memory_usage()
	var held = _gd_tree(root, DEPTH, LIMIT)
	var scripted_bytes := OS.get_static_memory_usage() - before
	held = null
	before = OS.get_static_memory_usage()
	held = native_snapshot.describe_tree(root, DEPTH, LIMIT)
	var native_bytes := OS.get_static_memory_usage() - before
	held = null

	# The capture path's own work: PNG encoding and base64 are engine calls
	# from either language.
	var image := Image.create(320, 240, false, Image.FORMAT_RGBA8)
	image.fill(Color(0.2, 0.4, 0.8))
	var encode := _time(func(): return Marshalls.raw_to_base64(image.save_png_to_buffer()))

	var identical := scripted_text == native_text and scripted_text == JSON.stringify(before_fix["result"])
	print("BENCH " + JSON.stringify({
		"godot": Engine.get_version_info()["string"],
		"nodes": (scripted["result"] as Array).size(),
		"identical": identical,
		"snapshot_sha256": scripted_text.sha256_text(),
		"gdscript_us": scripted["us"],
		"native_us": native["us"],
		"gdscript_pop_front_us": before_fix["us"],
		"gdscript_static_bytes": scripted_bytes,
		"native_static_bytes": native_bytes,
		"capture_encode_us": encode["us"],
	}))
	quit(0 if identical else 1)
