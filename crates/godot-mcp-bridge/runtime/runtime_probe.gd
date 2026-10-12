extends Node
## The runtime probe of one managed game session.
##
## The godot-mcp server starts the game with `runtime_launcher.gd`, which adds
## this node to the root before the project's scene. The probe dials the
## session's own bridge through `bridge_client.gd` — the same authenticated
## protocol the editor plugin speaks — and serves bounded reads of the running
## game and session-scoped synthetic input:
##
## * reads: the live scene tree, a node's properties as tagged values, its
##   signals and their connections (and, once watched, how often each was
##   emitted), the visible UI, the held input, and captures — a screenshot of
##   the root viewport after a drawn frame, or a recording of several — whose
##   PNG bytes the server reads back in bounded chunks;
## * mutations: synthetic actions, keys and mouse buttons, a timed sequence of
##   them, releasing everything held, and stopping the game.
##
## Every read answers with the probe's `revision` (it advances whenever a node
## enters, leaves or is renamed) and the engine `frame` it observed.
##
## Synthetic input belongs to this session. Every press has an expiry, and the
## probe releases what it holds when the press expires, when told to, when the
## session stops, when the bridge connection ends or the server falls silent —
## all decided here, without the server. A probe that has lost its server for
## longer than `ORPHAN_SECONDS` stops the game: a managed session never
## outlives the server that owns it.

const BridgeClient = preload("bridge_client.gd")
const VariantCodec = preload("variant_codec.gd")

const PROBE_VERSION := "0.2.0"
const RENDEZVOUS_ENV := "GODOT_MCP_RUNTIME_RENDEZVOUS"
const ORPHAN_SECONDS := 15.0
const MAX_NODES := 2000
const MAX_PROPERTIES := 256
const MAX_CONTROLS := 1000
const MAX_WATCHED_SIGNALS := 64
const DEFAULT_HOLD_MS := 500
const MAX_HOLD_MS := 30000
const MAX_SEQUENCE_STEPS := 64
const MAX_SEQUENCE_MS := 10000
const WAIT_SLICE_SECONDS := 0.05
const MAX_RECORDING_MS := 10000
const MAX_RECORDING_FPS := 30
const MAX_RECORDING_FRAMES := 300
const MAX_CAPTURE_BYTES := 67108864
const MAX_CHUNK_BYTES := 1048576
const MAX_CAPTURES := 2
const SKIPPED_USAGE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_SUBGROUP

const READS := [
	"bridge.describe_session",
	"bridge.ping",
	"runtime.capture.chunk",
	"runtime.capture.recording",
	"runtime.capture.screenshot",
	"runtime.input.state",
	"runtime.properties",
	"runtime.signals",
	"runtime.tree",
	"runtime.ui",
]
const MUTATIONS := [
	"runtime.input.release",
	"runtime.input.send",
	"runtime.input.sequence",
	"runtime.session.stop",
]
const MOUSE_BUTTONS := {
	"left": MOUSE_BUTTON_LEFT,
	"right": MOUSE_BUTTON_RIGHT,
	"middle": MOUSE_BUTTON_MIDDLE,
	"wheel_up": MOUSE_BUTTON_WHEEL_UP,
	"wheel_down": MOUSE_BUTTON_WHEEL_DOWN,
}

var _client: Node = null
var _revision := 0
## Held synthetic input by key ("action:<name>", "key:<code>", "mouse:<button>")
## to {"event": the pressed InputEvent, "expires_ms": int}.
var _held: Dictionary = {}
var _mouse_position := Vector2.ZERO
## "<node path>|<signal>" to emission count, for watched signals.
var _emissions: Dictionary = {}
var _captures: Dictionary = {}
var _capture_order: Array = []
var _next_capture := 1
var _cancelled: Dictionary = {}
var _orphaned_since := -1.0
var _served := false
var _stopping := false


func _ready() -> void:
	process_mode = Node.PROCESS_MODE_ALWAYS
	var tree := get_tree()
	tree.node_added.connect(_on_tree_changed)
	tree.node_removed.connect(_on_tree_changed)
	tree.node_renamed.connect(_on_tree_changed)
	_client = BridgeClient.new()
	_client.name = "GodotMcpRuntimeBridge"
	_client.handlers = self
	_client.client_version = PROBE_VERSION
	_client.rendezvous_env = RENDEZVOUS_ENV
	_client.rendezvous_path = ""
	_client.process_mode = Node.PROCESS_MODE_ALWAYS
	_client.disconnected.connect(_on_disconnected)
	add_child(_client)


func _exit_tree() -> void:
	_release_all()


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_CLOSE_REQUEST or what == NOTIFICATION_PREDELETE:
		_release_all()


func _on_tree_changed(_node: Node) -> void:
	_revision += 1


func _on_disconnected(_reason: String) -> void:
	# Whatever the server pressed is released the moment it loses sight of it.
	_release_all()


static func _now_ms() -> int:
	return Time.get_ticks_msec()


func _process(_delta: float) -> void:
	var now := _now_ms()
	for key in _held.keys():
		if int(_held[key]["expires_ms"]) <= now:
			_release(key)
	if _client == null:
		return
	if _client.is_connected_to_server():
		_served = true
		_orphaned_since = -1.0
	elif _served and not _stopping:
		if _orphaned_since < 0.0:
			_orphaned_since = now / 1000.0
		elif now / 1000.0 - _orphaned_since > ORPHAN_SECONDS:
			_release_all()
			_stopping = true
			get_tree().quit(0)


# --- the handler-set interface the command queue calls ------------------------


func methods() -> Array:
	var all: Array = READS.duplicate()
	all.append_array(MUTATIONS)
	all.sort()
	return all


func supports(method: String) -> bool:
	return READS.has(method) or MUTATIONS.has(method)


func is_mutation(method: String) -> bool:
	return MUTATIONS.has(method)


func cancel_running(message_id: String) -> void:
	_cancelled[message_id] = true


static func ok(payload: Dictionary, status: String = "not_started") -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": status}


static func fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


func dispatch(method: String, payload: Dictionary, context: Dictionary) -> Dictionary:
	match method:
		"bridge.describe_session":
			return ok(describe())
		"bridge.ping":
			return ok({"pong": true})
		"runtime.tree":
			return _tree(payload)
		"runtime.properties":
			return _properties(payload)
		"runtime.signals":
			return _signals(payload)
		"runtime.ui":
			return _ui(payload)
		"runtime.input.state":
			return ok(_observed({"held": _held_snapshot()}))
		"runtime.input.send":
			return _input_send(payload)
		"runtime.input.release":
			var released: Array = _held_snapshot()
			_release_all()
			return ok(_observed({"released": released, "held": []}), "committed")
		"runtime.input.sequence":
			return await _sequence(payload, str(context.get("message_id", "")))
		"runtime.capture.screenshot":
			return await _screenshot()
		"runtime.capture.recording":
			return await _recording(payload, str(context.get("message_id", "")))
		"runtime.capture.chunk":
			return _chunk(payload)
		"runtime.session.stop":
			return _stop(payload)
	return fail("unsupported", "the runtime probe does not implement %s" % method)


func describe() -> Dictionary:
	var version: Dictionary = Engine.get_version_info()
	var size: Vector2i = get_tree().root.size
	var scene: Node = get_tree().current_scene
	return {
		"probe_version": PROBE_VERSION,
		"godot_version": str(version.get("string", "")),
		"methods": methods(),
		"display": DisplayServer.get_name(),
		"renders": DisplayServer.get_name() != "headless",
		"rendering_method": str(ProjectSettings.get_setting("rendering/renderer/rendering_method", "")),
		"window": {"width": size.x, "height": size.y},
		"current_scene": str(scene.scene_file_path) if scene != null else "",
	}


func _observed(payload: Dictionary) -> Dictionary:
	payload["revision"] = _revision
	payload["frame"] = Engine.get_process_frames()
	return payload


# --- reads -------------------------------------------------------------------


func _own(node: Node) -> bool:
	return node == self or is_ancestor_of(node)


## A node of the running game: an absolute path (`/root/...`), or a path
## relative to the current scene ("" or "." is the current scene itself).
func _resolve(reference) -> Node:
	var spelled := str(reference)
	var tree := get_tree()
	var node: Node = null
	if spelled.begins_with("/"):
		node = tree.root.get_node_or_null(NodePath(spelled))
	elif tree.current_scene != null:
		node = tree.current_scene if spelled == "" or spelled == "." else tree.current_scene.get_node_or_null(NodePath(spelled))
	if node == null or _own(node):
		return null
	return node


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


func _tree(payload: Dictionary) -> Dictionary:
	var start: Node = get_tree().root
	if str(payload.get("root", "")) not in ["", "/root"]:
		start = _resolve(payload["root"])
		if start == null:
			return fail("not_found", "%s names no node of the running game" % str(payload["root"]))
	var depth: int = clampi(int(payload.get("depth", 8)), 0, 64)
	var limit: int = clampi(int(payload.get("limit", 500)), 1, MAX_NODES)
	var nodes: Array = []
	var truncated := false
	# A cursor rather than pop_front(), which shifts the whole array on every
	# step and makes a large tree's walk quadratic.
	var pending: Array = [[start, 0]]
	var cursor := 0
	while cursor < pending.size():
		var item: Array = pending[cursor]
		cursor += 1
		var node: Node = item[0]
		if _own(node):
			continue
		if nodes.size() >= limit:
			truncated = true
			break
		nodes.append(_describe_node(node))
		if int(item[1]) < depth:
			for child in node.get_children():
				pending.append([child, int(item[1]) + 1])
		elif node.get_child_count() > 0:
			truncated = true
	var scene: Node = get_tree().current_scene
	return ok(_observed({
		"current_scene": str(scene.get_path()) if scene != null else null,
		"nodes": nodes,
		"truncated": truncated,
	}))


func _properties(payload: Dictionary) -> Dictionary:
	var node: Node = _resolve(payload.get("node", ""))
	if node == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("node", "")))
	var wanted = payload.get("properties", null)
	if wanted != null and (typeof(wanted) != TYPE_ARRAY or wanted.size() > MAX_PROPERTIES):
		return fail("invalid_input", "`properties` must be an array of at most %d names" % MAX_PROPERTIES)
	var values := {}
	var unrepresentable := {}
	var truncated := false
	var names: Array = []
	if wanted != null:
		for name in wanted:
			names.append(str(name))
	else:
		for property in node.get_property_list():
			var usage: int = int(property.get("usage", 0))
			if usage & SKIPPED_USAGE or not (usage & (PROPERTY_USAGE_STORAGE | PROPERTY_USAGE_EDITOR)):
				continue
			if names.size() >= MAX_PROPERTIES:
				truncated = true
				break
			names.append(str(property["name"]))
	var known := {}
	for property in node.get_property_list():
		known[str(property["name"])] = true
	for name in names:
		if not known.has(name):
			return fail("not_found", "%s has no property %s" % [str(node.get_path()), name])
		var encoded: Dictionary = VariantCodec.encode(node.get(name))
		if encoded.get("ok", false):
			values[name] = encoded["value"]
		else:
			unrepresentable[name] = str(encoded.get("reason", "not representable"))
	return ok(_observed({
		"node": str(node.get_path()),
		"class": node.get_class(),
		"properties": values,
		"unrepresentable": unrepresentable,
		"truncated": truncated,
	}))


func _on_emitted(key: String) -> void:
	_emissions[key] = int(_emissions.get(key, 0)) + 1


func _signals(payload: Dictionary) -> Dictionary:
	var node: Node = _resolve(payload.get("node", ""))
	if node == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("node", "")))
	var watch := bool(payload.get("watch", false))
	var signals: Array = []
	for info in node.get_signal_list():
		var name := str(info["name"])
		var key := "%s|%s" % [str(node.get_path()), name]
		var arguments: Array = info.get("args", [])
		if watch and not _emissions.has(key):
			if _emissions.size() >= MAX_WATCHED_SIGNALS:
				return fail("invalid_input", "at most %d signals can be watched in one session" % MAX_WATCHED_SIGNALS)
			_emissions[key] = 0
			node.connect(name, _on_emitted.bind(key).unbind(arguments.size()))
		var connections: Array = []
		for connection in node.get_signal_connection_list(name):
			var callable: Callable = connection["callable"]
			var target: Object = callable.get_object()
			if target == self:
				continue
			connections.append({
				"target": str(target.get_path()) if target is Node else (target.get_class() if target != null else null),
				"method": str(callable.get_method()),
				"flags": int(connection.get("flags", 0)),
			})
		var argument_names: Array = []
		for argument in arguments:
			argument_names.append(str(argument.get("name", "")))
		signals.append({
			"name": name,
			"arguments": argument_names,
			"connections": connections,
			"emissions": _emissions.get(key, null),
		})
	return ok(_observed({"node": str(node.get_path()), "signals": signals}))


func _ui(payload: Dictionary) -> Dictionary:
	var limit: int = clampi(int(payload.get("limit", 500)), 1, MAX_CONTROLS)
	var include_hidden := bool(payload.get("include_hidden", false))
	var focus: Control = get_viewport().gui_get_focus_owner()
	var controls: Array = []
	var truncated := false
	var pending: Array = [get_tree().root]
	var cursor := 0
	while cursor < pending.size():
		var node: Node = pending[cursor]
		cursor += 1
		if _own(node):
			continue
		for child in node.get_children():
			pending.append(child)
		if not (node is Control):
			continue
		var control := node as Control
		if not include_hidden and not control.is_visible_in_tree():
			continue
		if controls.size() >= limit:
			truncated = true
			break
		var rect: Rect2 = control.get_global_rect()
		var entry := {
			"path": str(control.get_path()),
			"class": control.get_class(),
			"visible": control.is_visible_in_tree(),
			"rect": {"x": rect.position.x, "y": rect.position.y, "width": rect.size.x, "height": rect.size.y},
			"focused": control == focus,
		}
		if "text" in control:
			entry["text"] = str(control.get("text"))
		if control is BaseButton:
			entry["disabled"] = (control as BaseButton).disabled
			entry["pressed"] = (control as BaseButton).button_pressed
		controls.append(entry)
	return ok(_observed({
		"focus": str(focus.get_path()) if focus != null else null,
		"controls": controls,
		"truncated": truncated,
	}))


# --- synthetic input -----------------------------------------------------------


func _held_snapshot() -> Array:
	var held: Array = []
	for key in _held.keys():
		held.append({"input": key, "expires_in_ms": max(0, int(_held[key]["expires_ms"]) - _now_ms())})
	held.sort_custom(func(a, b): return str(a["input"]) < str(b["input"]))
	return held


func _release(key: String) -> void:
	if not _held.has(key):
		return
	var pressed: InputEvent = _held[key]["event"]
	_held.erase(key)
	var released: InputEvent = pressed.duplicate()
	if released is InputEventAction:
		(released as InputEventAction).pressed = false
		(released as InputEventAction).strength = 0.0
		Input.parse_input_event(released)
		Input.action_release((released as InputEventAction).action)
	elif released is InputEventKey:
		(released as InputEventKey).pressed = false
		Input.parse_input_event(released)
	elif released is InputEventMouseButton:
		(released as InputEventMouseButton).pressed = false
		(released as InputEventMouseButton).position = _mouse_position
		(released as InputEventMouseButton).global_position = _mouse_position
		Input.parse_input_event(released)


func _release_all() -> void:
	for key in _held.keys():
		_release(key)


static func _position(value) -> Variant:
	if typeof(value) != TYPE_DICTIONARY or not value.has("x") or not value.has("y"):
		return null
	return Vector2(float(value["x"]), float(value["y"]))


## Build the input event one request describes, with the key it is held
## under. Returns {"event", "key"} or {"error": refusal}.
func _event_for(step: Dictionary) -> Dictionary:
	var kind := str(step.get("kind", ""))
	var pressed := str(step.get("state", "tap")) != "release"
	match kind:
		"action":
			var action := str(step.get("action", ""))
			if not InputMap.has_action(action):
				return {"error": fail("not_found", "the game has no input action %s" % action)}
			var event := InputEventAction.new()
			event.action = action
			event.pressed = pressed
			event.strength = clampf(float(step.get("strength", 1.0)), 0.0, 1.0) if pressed else 0.0
			return {"event": event, "key": "action:%s" % action}
		"key":
			var keycode: Key = OS.find_keycode_from_string(str(step.get("key", "")))
			if keycode == KEY_NONE:
				return {"error": fail("invalid_input", "%s is not a key name" % str(step.get("key", "")))}
			var event := InputEventKey.new()
			event.keycode = keycode
			event.physical_keycode = keycode
			event.pressed = pressed
			var modifiers = step.get("modifiers", {})
			if typeof(modifiers) == TYPE_DICTIONARY:
				event.shift_pressed = bool(modifiers.get("shift", false))
				event.ctrl_pressed = bool(modifiers.get("ctrl", false))
				event.alt_pressed = bool(modifiers.get("alt", false))
				event.meta_pressed = bool(modifiers.get("meta", false))
			return {"event": event, "key": "key:%d" % keycode}
		"mouse_button":
			var name := str(step.get("button", "left"))
			if not MOUSE_BUTTONS.has(name):
				return {"error": fail("invalid_input", "%s is not a mouse button" % name)}
			var at = _position(step.get("position"))
			if at != null:
				_move_mouse(at)
			var event := InputEventMouseButton.new()
			event.button_index = MOUSE_BUTTONS[name]
			event.pressed = pressed
			event.position = _mouse_position
			event.global_position = _mouse_position
			return {"event": event, "key": "mouse:%s" % name}
		"mouse_motion":
			var at = _position(step.get("position"))
			if at == null:
				return {"error": fail("invalid_input", "a mouse motion needs `position` {x, y}")}
			return {"motion": at}
	return {"error": fail("invalid_input", "%s is not an input kind" % kind)}


func _move_mouse(to: Vector2) -> void:
	var motion := InputEventMouseMotion.new()
	motion.position = to
	motion.global_position = to
	motion.relative = to - _mouse_position
	_mouse_position = to
	Input.parse_input_event(motion)


## Apply one input step. `state` is "press" (held until released or expired),
## "release", or "tap" (pressed now, released after `hold_ms`).
func _apply(step: Dictionary) -> Dictionary:
	var built: Dictionary = _event_for(step)
	if built.has("error"):
		return built["error"]
	if built.has("motion"):
		_move_mouse(built["motion"])
		return ok({})
	var key: String = built["key"]
	var state := str(step.get("state", "tap"))
	if state not in ["press", "release", "tap"]:
		return fail("invalid_input", "`state` must be press, release or tap")
	if state == "release":
		_release(key)
		return ok({})
	var hold: int = clampi(int(step.get("hold_ms", DEFAULT_HOLD_MS)), 1, MAX_HOLD_MS)
	if _held.has(key):
		_release(key)
	Input.parse_input_event(built["event"])
	_held[key] = {"event": built["event"], "expires_ms": _now_ms() + hold}
	return ok({})


func _input_send(payload: Dictionary) -> Dictionary:
	var applied: Dictionary = _apply(payload)
	if not applied.get("ok", false):
		return applied
	return ok(_observed({"held": _held_snapshot()}), "committed")


func _sequence(payload: Dictionary, message_id: String) -> Dictionary:
	var steps = payload.get("steps")
	if typeof(steps) != TYPE_ARRAY or steps.is_empty() or steps.size() > MAX_SEQUENCE_STEPS:
		return fail("invalid_input", "`steps` must be an array of 1 to %d steps" % MAX_SEQUENCE_STEPS)
	var waited := 0
	for step in steps:
		if typeof(step) != TYPE_DICTIONARY:
			return fail("invalid_input", "every step must be an object")
		if str(step.get("kind", "")) == "wait":
			waited += int(step.get("ms", 0))
	if waited > MAX_SEQUENCE_MS:
		return fail("invalid_input", "a sequence may wait at most %d ms in total" % MAX_SEQUENCE_MS)
	var before: Dictionary = _held.duplicate()
	var run := 0
	var cancelled := false
	var failure: Dictionary = {}
	for step in steps:
		if _cancelled.has(message_id) or _stopping:
			cancelled = true
			break
		if str(step.get("kind", "")) == "wait":
			var remaining: float = clampi(int(step.get("ms", 0)), 0, MAX_SEQUENCE_MS) / 1000.0
			# A wait is sliced, so a withdrawal or a stop ends it within one slice
			# and what the sequence pressed is released then, not when it would
			# have ended.
			while remaining > 0.0 and not (_cancelled.has(message_id) or _stopping):
				var slice: float = minf(remaining, WAIT_SLICE_SECONDS)
				await get_tree().create_timer(slice, true, false, true).timeout
				remaining -= slice
		else:
			var applied: Dictionary = _apply(step)
			if not applied.get("ok", false):
				failure = applied
				break
		run += 1
	_cancelled.erase(message_id)
	# A sequence leaves nothing held that it pressed itself.
	for key in _held.keys():
		if not before.has(key):
			_release(key)
	var status := "committed" if run > 0 else "not_started"
	if not failure.is_empty():
		failure["message"] = "%s (after %d of %d steps; everything the sequence pressed was released)" % [failure["message"], run, steps.size()]
		failure["mutation_status"] = status
		return failure
	return ok(_observed({"steps_run": run, "cancelled": cancelled, "held": _held_snapshot()}), status)


# --- captures --------------------------------------------------------------------


func _keep(capture: Dictionary) -> Dictionary:
	var id := "capture-%d" % _next_capture
	_next_capture += 1
	_captures[id] = capture
	_capture_order.append(id)
	while _capture_order.size() > MAX_CAPTURES:
		_captures.erase(_capture_order.pop_front())
	var frames: Array = []
	for png in capture["frames"]:
		var hashing := HashingContext.new()
		hashing.start(HashingContext.HASH_SHA256)
		hashing.update(png)
		frames.append({"byte_length": png.size(), "sha256": hashing.finish().hex_encode()})
	return {
		"capture_id": id,
		"width": capture["width"],
		"height": capture["height"],
		"frames": frames,
		"fps": capture.get("fps", null),
	}


func _grab(max_width: int) -> Image:
	var image: Image = get_viewport().get_texture().get_image()
	if image == null or image.is_empty():
		return null
	if max_width > 0 and image.get_width() > max_width:
		var height: int = maxi(1, int(round(float(image.get_height()) * max_width / image.get_width())))
		image.resize(max_width, height, Image.INTERPOLATE_BILINEAR)
	return image


func _screenshot() -> Dictionary:
	if DisplayServer.get_name() == "headless":
		return fail("unavailable", "this session renders nothing: it runs headless")
	await RenderingServer.frame_post_draw
	var image: Image = _grab(0)
	if image == null:
		return fail("unavailable", "the root viewport produced no image")
	var png: PackedByteArray = image.save_png_to_buffer()
	var kept: Dictionary = _keep({"frames": [png], "width": image.get_width(), "height": image.get_height()})
	return ok(_observed(kept))


func _recording(payload: Dictionary, message_id: String) -> Dictionary:
	if DisplayServer.get_name() == "headless":
		return fail("unavailable", "this session renders nothing: it runs headless")
	var duration: int = int(payload.get("duration_ms", 1000))
	var fps: int = int(payload.get("fps", 10))
	if duration < 1 or duration > MAX_RECORDING_MS or fps < 1 or fps > MAX_RECORDING_FPS:
		return fail("invalid_input", "a recording lasts 1 to %d ms at 1 to %d frames per second" % [MAX_RECORDING_MS, MAX_RECORDING_FPS])
	var max_width: int = clampi(int(payload.get("max_width", 0)), 0, 4096)
	var wanted: int = mini(MAX_RECORDING_FRAMES, maxi(1, int(ceil(duration * fps / 1000.0))))
	var interval: float = 1000.0 / fps
	var frames: Array = []
	var total := 0
	var width := 0
	var height := 0
	var next_at := float(_now_ms())
	while frames.size() < wanted:
		await RenderingServer.frame_post_draw
		if _stopping:
			return fail("unavailable", "the session stopped during the recording")
		if _cancelled.has(message_id):
			# A withdrawn recording keeps none of its frames.
			_cancelled.erase(message_id)
			return fail("cancelled", "the recording was withdrawn; none of its frames were kept")
		if float(_now_ms()) < next_at:
			continue
		next_at += interval
		var image: Image = _grab(max_width)
		if image == null:
			return fail("unavailable", "the root viewport produced no image")
		var png: PackedByteArray = image.save_png_to_buffer()
		total += png.size()
		if total > MAX_CAPTURE_BYTES:
			return fail("invalid_input", "the recording exceeds %d bytes; record less or smaller" % MAX_CAPTURE_BYTES)
		width = image.get_width()
		height = image.get_height()
		frames.append(png)
	var kept: Dictionary = _keep({"frames": frames, "width": width, "height": height, "fps": fps})
	return ok(_observed(kept))


func _chunk(payload: Dictionary) -> Dictionary:
	var id := str(payload.get("capture_id", ""))
	if not _captures.has(id):
		return fail("not_found", "%s is not a capture this session still holds" % id)
	var frames: Array = _captures[id]["frames"]
	var index: int = int(payload.get("frame", 0))
	if index < 0 or index >= frames.size():
		return fail("invalid_input", "frame %d is outside the capture's %d frames" % [index, frames.size()])
	var bytes: PackedByteArray = frames[index]
	var offset: int = int(payload.get("offset", 0))
	var length: int = clampi(int(payload.get("length", MAX_CHUNK_BYTES)), 1, MAX_CHUNK_BYTES)
	if offset < 0 or offset > bytes.size():
		return fail("invalid_input", "offset %d is outside the frame's %d bytes" % [offset, bytes.size()])
	var slice: PackedByteArray = bytes.slice(offset, offset + length)
	return ok({
		"capture_id": id,
		"frame": index,
		"offset": offset,
		"length": slice.size(),
		"total": bytes.size(),
		"data_base64": Marshalls.raw_to_base64(slice),
	})


# --- stopping ----------------------------------------------------------------------


func _stop(payload: Dictionary) -> Dictionary:
	var released: Array = _held_snapshot()
	_release_all()
	_stopping = true
	_quit_soon.call_deferred(clampi(int(payload.get("exit_code", 0)), 0, 125))
	return ok(_observed({"stopping": true, "released": released}), "committed")


func _quit_soon(code: int) -> void:
	# Let the bridge send the answer before the game exits.
	for frame in 3:
		await get_tree().process_frame
	get_tree().quit(code)
