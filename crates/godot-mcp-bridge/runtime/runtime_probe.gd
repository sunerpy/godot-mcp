extends Node
## The runtime probe of one managed game session.
##
## The godot-mcp server starts the game with `runtime_launcher.gd`, which adds
## this node to the root before the project's scene. The probe dials the
## session's own bridge through `bridge_client.gd` — the same authenticated
## protocol the editor plugin speaks — and serves bounded reads of the running
## game and session-scoped synthetic input:
##
## * reads: the live scene tree, a node's properties as tagged values (or
##   several nodes' at once), its signals and their connections (and, once
##   watched, how often each was emitted), the visible UI, the held input,
##   node searches by class, script, group, name and distance, navigation
##   paths, waits for a condition, timed samples of properties and monitors,
##   captures — a screenshot of the root viewport after a drawn frame, or a
##   recording of several — whose PNG bytes the server reads back in bounded
##   chunks, a capture compared with an uploaded reference image, and the
##   checks of a test scenario's assertions;
## * mutations: property writes (converted without loss, then read back),
##   synthetic actions, keys and mouse buttons, a timed sequence of them, a
##   replay of recorded input, a click on a button, moving a node, recording
##   the input the game receives, releasing everything held, running a
##   caller's script when the server offers it (`script_runner.gd`), a test
##   scenario's stress window, and stopping the game.
##
## Every read answers with the probe's `revision` (it advances whenever a node
## enters, leaves or is renamed) and the engine `frame` it observed.
##
## Synthetic input belongs to this session. Every press has an expiry, and the
## probe releases what it holds when the press expires, when told to, when the
## session stops, when the bridge connection ends or the server falls silent —
## all decided here, without the server. A probe that has lost its server for
## longer than `ORPHAN_SECONDS` stops the game: a managed session never
## outlives the server that owns it. Every event the probe injects carries the
## `PROBE_META` marker, so an input recording says which events came from it.
##
## Requests run one at a time. A request that spans frames — a wait, a sample,
## a replay, a move, a click — ends at its next frame when the server withdraws
## it, when the session stops or when the connection is lost.
##
## What differs between the Godot builds the server supports (4.4.1 to 4.7.1)
## is probed once, in `_probe_compatibility`, and read from `_compat`.

const BridgeClient = preload("bridge_client.gd")
const VariantCodec = preload("variant_codec.gd")
const Values = preload("runtime_values.gd")
const Events = preload("runtime_events.gd")
const ScriptRunner = preload("script_runner.gd")
const EditorLog = preload("editor_log.gd")

const PROBE_VERSION := "0.4.0"
const RENDEZVOUS_ENV := "GODOT_MCP_RUNTIME_RENDEZVOUS"
const PROBE_META := &"godot_mcp_probe"
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
const MAX_SET_PROPERTIES := 32
const MAX_BATCH_TARGETS := 32
const MAX_BATCH_PROPERTIES := 64
## Roughly how many bytes of values one batch read or one sample answers,
## well inside the bridge's 4 MiB frame.
const MAX_ANSWER_BYTES := 2500000
const MAX_WAIT_MS := 30000
const MAX_WAIT_SIGNAL_COUNT := 1000
const MAX_SAMPLE_MS := 10000
const MAX_SAMPLE_INTERVAL_MS := 1000
const MAX_SAMPLES := 600
const MAX_SERIES := 32
const MAX_FOUND := 500
const MAX_SCANNED := 20000
const MAX_INPUT_EVENTS := 3000
const MAX_INPUT_RECORDING_MS := 30000
const MAX_REPLAY_MS := 30000
const MAX_SYNC_MS := 5000
const MAX_MOVE_MS := 30000
const MAX_SPEED := 100000.0
const MAX_COORDINATE := 1000000000.0
const MAX_PRESS_HOLD_MS := 5000
const DEFAULT_PRESS_HOLD_MS := 100
## How many physics frames without progress end a move as blocked.
const BLOCKED_FRAMES := 30
## How many physics frames an empty path waits for the map's next iteration.
const RETRY_FRAMES := 10
const SKIPPED_USAGE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_SUBGROUP
## The singleton properties a write may change, each with its type and range.
const SINGLETON_PROPERTIES := {
	"Engine": {
		"time_scale": [TYPE_FLOAT, 0.0, 100.0],
		"physics_ticks_per_second": [TYPE_INT, 1.0, 1000.0],
		"max_fps": [TYPE_INT, 0.0, 1000.0],
	},
}

const READS := [
	"bridge.describe_session",
	"bridge.ping",
	"runtime.capture.chunk",
	"runtime.capture.recording",
	"runtime.capture.screenshot",
	"runtime.image.compare",
	"runtime.image.reference",
	"runtime.input.state",
	"runtime.navigation.path",
	"runtime.nodes.find",
	"runtime.performance",
	"runtime.properties",
	"runtime.properties.batch",
	"runtime.sample",
	"runtime.scenario.check",
	"runtime.signals",
	"runtime.tree",
	"runtime.ui",
	"runtime.wait",
]
const MUTATIONS := [
	"runtime.input.record.start",
	"runtime.input.record.stop",
	"runtime.input.release",
	"runtime.input.replay",
	"runtime.input.send",
	"runtime.input.sequence",
	"runtime.node.move",
	"runtime.properties.set",
	"runtime.scenario.stress",
	"runtime.script.run",
	"runtime.session.stop",
	"runtime.ui.press",
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
## Held synthetic input by key ("action:<name>", "key:<code>", "mouse:<button>",
## "joy:<device>:<button>", "joyaxis:<device>:<axis>") to {"event": the pressed
## InputEvent, "expires_ms": int}.
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
## Advances whenever the bridge connection ends, so a request spanning frames
## sees that its server is gone.
var _epoch := 0
## What this Godot build offers, probed once.
var _compat: Dictionary = {}
## The input recording, while one is kept: {"id", "started_ms", "max_events",
## "max_ms", "events", "dropped", "ended_by", "active"}.
var _recording: Dictionary = {}
var _next_input_recording := 1
## Native property names by class, for the read-only check.
var _native_properties: Dictionary = {}
## The methods the server listed for the current bridge session.
var _server_methods: Array = []
var _scripts = ScriptRunner.new()
## What the engine logs, captured from the first script run on (Godot 4.5 and
## later), so a run's report says what it printed and raised.
var _log = null


func _ready() -> void:
	process_mode = Node.PROCESS_MODE_ALWAYS
	_probe_compatibility()
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
	_client.session_opened.connect(_on_session_opened)
	add_child(_client)


## Every difference between the supported Godot builds the probe depends on,
## in one place:
##
## * `region_iteration_2d` / `_3d`: NavigationServer2D/3D.region_get_iteration_id,
##   added in 4.5, when a region's navigation mesh started to sync on its own,
##   one map iteration before the map path-finds over it; on 4.4 a map that has
##   iterated once path-finds over its regions;
## * `window_focus`: whether the display server reports window focus; a
##   headless one reports every window focused.
func _probe_compatibility() -> void:
	_compat = {
		"region_iteration_2d": NavigationServer2D.has_method("region_get_iteration_id"),
		"region_iteration_3d": NavigationServer3D.has_method("region_get_iteration_id"),
		"window_focus": DisplayServer.get_name() != "headless",
	}


func _exit_tree() -> void:
	_release_all()
	if _log != null:
		_log.stop()
		_log = null


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_CLOSE_REQUEST or what == NOTIFICATION_PREDELETE:
		_release_all()


func _on_tree_changed(_node: Node) -> void:
	_revision += 1


func _on_session_opened(summary: Dictionary) -> void:
	var methods = summary.get("methods", [])
	_server_methods = methods.duplicate() if typeof(methods) == TYPE_ARRAY else []


func _on_disconnected(_reason: String) -> void:
	# Whatever the server pressed is released the moment it loses sight of it,
	# and every request spanning frames ends.
	_epoch += 1
	_release_all()


static func _now_ms() -> int:
	return Time.get_ticks_msec()


func _process(_delta: float) -> void:
	var now := _now_ms()
	for key in _held.keys():
		if int(_held[key]["expires_ms"]) <= now:
			_release(key)
	if _recording.get("active", false) and now - int(_recording["started_ms"]) > int(_recording["max_ms"]):
		_end_recording("max_duration")
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
	var message_id := str(context.get("message_id", ""))
	match method:
		"bridge.describe_session":
			return ok(describe())
		"bridge.ping":
			return ok({"pong": true})
		"runtime.tree":
			return _tree(payload)
		"runtime.properties":
			return _properties(payload)
		"runtime.properties.batch":
			return _properties_batch(payload)
		"runtime.properties.set":
			return _properties_set(payload)
		"runtime.signals":
			return _signals(payload)
		"runtime.ui":
			return _ui(payload)
		"runtime.ui.press":
			return await _press(payload, message_id)
		"runtime.performance":
			return _performance(payload)
		"runtime.nodes.find":
			return _find(payload)
		"runtime.navigation.path":
			return await _navigation_path(payload, message_id)
		"runtime.node.move":
			return await _move(payload, message_id)
		"runtime.wait":
			return await _wait(payload, message_id)
		"runtime.sample":
			return await _sample(payload, message_id)
		"runtime.input.state":
			return ok(_observed({"held": _held_snapshot()}))
		"runtime.input.send":
			return _input_send(payload)
		"runtime.input.release":
			var released: Array = _held_snapshot()
			_release_all()
			return ok(_observed({"released": released, "held": []}), "committed")
		"runtime.input.sequence":
			return await _sequence(payload, message_id)
		"runtime.input.record.start":
			return _record_start(payload)
		"runtime.input.record.stop":
			return _record_stop()
		"runtime.input.replay":
			return await _replay(payload, message_id)
		"runtime.capture.screenshot":
			return await _screenshot()
		"runtime.capture.recording":
			return await _recording_capture(payload, message_id)
		"runtime.capture.chunk":
			return _chunk(payload)
		"runtime.image.reference":
			return _image_reference(payload)
		"runtime.image.compare":
			return await _image_compare(payload, message_id)
		"runtime.scenario.check":
			return _scenario_check(payload)
		"runtime.scenario.stress":
			return await _scenario_stress(payload, message_id)
		"runtime.script.run":
			return await _script_run(payload, message_id)
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


## Whether a request spanning frames must end now: the server withdrew it,
## the session is stopping, or the connection it came on is gone.
func _interrupted(message_id: String, epoch: int) -> String:
	if _cancelled.has(message_id):
		return "cancelled"
	if _stopping:
		return "stopping"
	if epoch != _epoch:
		return "disconnected"
	return ""


static func _json_size(value) -> int:
	return JSON.stringify(value).length()


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


## The properties of `node` an answer lists: the `wanted` names, or its stored
## and editor-visible ones up to `limit`. Returns {"names", "truncated",
## "missing"}: `missing` holds wanted names the node does not have.
func _property_names(node: Object, wanted, limit: int) -> Dictionary:
	var names: Array = []
	var missing: Array = []
	var truncated := false
	var known := {}
	for property in node.get_property_list():
		known[str(property["name"])] = true
	if wanted != null:
		for name in wanted:
			if known.has(str(name)):
				names.append(str(name))
			else:
				missing.append(str(name))
	else:
		for property in node.get_property_list():
			var usage: int = int(property.get("usage", 0))
			if usage & SKIPPED_USAGE or not (usage & (PROPERTY_USAGE_STORAGE | PROPERTY_USAGE_EDITOR)):
				continue
			if names.size() >= limit:
				truncated = true
				break
			names.append(str(property["name"]))
	return {"names": names, "truncated": truncated, "missing": missing}


## The tagged values of `names` on `object`, with the reason for each value
## the contract cannot carry.
static func _encode_properties(object: Object, names: Array) -> Dictionary:
	var values := {}
	var unrepresentable := {}
	for name in names:
		var encoded: Dictionary = VariantCodec.encode(object.get(name))
		if encoded.get("ok", false):
			values[name] = encoded["value"]
		else:
			unrepresentable[name] = str(encoded.get("reason", "not representable"))
	return {"properties": values, "unrepresentable": unrepresentable}


func _properties(payload: Dictionary) -> Dictionary:
	var node: Node = _resolve(payload.get("node", ""))
	if node == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("node", "")))
	var wanted = payload.get("properties", null)
	if wanted != null and (typeof(wanted) != TYPE_ARRAY or wanted.size() > MAX_PROPERTIES):
		return fail("invalid_input", "`properties` must be an array of at most %d names" % MAX_PROPERTIES)
	var listed: Dictionary = _property_names(node, wanted, MAX_PROPERTIES)
	if not listed["missing"].is_empty():
		return fail("not_found", "%s has no property %s" % [str(node.get_path()), listed["missing"][0]])
	var encoded: Dictionary = _encode_properties(node, listed["names"])
	return ok(_observed({
		"node": str(node.get_path()),
		"class": node.get_class(),
		"properties": encoded["properties"],
		"unrepresentable": encoded["unrepresentable"],
		"truncated": listed["truncated"],
	}))


## The object one request names: {"object", "name", "class", "singleton"} or
## {"error": refusal}. A request names a `node` of the game or a `singleton`
## whose properties the probe allows.
func _target_of(item: Dictionary) -> Dictionary:
	var has_node := item.has("node")
	var has_singleton := item.has("singleton")
	if has_node == has_singleton:
		return {"error": fail("invalid_input", "name either a `node` or a `singleton`")}
	if has_singleton:
		var singleton := str(item["singleton"])
		if not SINGLETON_PROPERTIES.has(singleton):
			return {"error": fail("invalid_input", "%s is not a singleton the probe reads or writes; only %s are" % [
				singleton, ", ".join(SINGLETON_PROPERTIES.keys())])}
		# SINGLETON_PROPERTIES names Engine alone.
		return {"object": Engine, "name": singleton, "class": singleton, "singleton": true}
	var node: Node = _resolve(item["node"])
	if node == null:
		return {"error": fail("not_found", "%s names no node of the running game" % str(item["node"]))}
	return {"object": node, "name": str(node.get_path()), "class": node.get_class(), "singleton": false}


## One target of a batch read: its values, or why it has none.
func _read_target(item) -> Dictionary:
	if typeof(item) != TYPE_DICTIONARY:
		return {"error": {"code": "invalid_input", "message": "a target is an object"}}
	var target: Dictionary = _target_of(item)
	var label := {"singleton": item["singleton"]} if item.has("singleton") else {"node": str(item.get("node", ""))}
	if target.has("error"):
		label["error"] = {"code": target["error"]["error_kind"], "message": target["error"]["message"]}
		return label
	var wanted = item.get("properties", null)
	if wanted != null and (typeof(wanted) != TYPE_ARRAY or wanted.size() > MAX_BATCH_PROPERTIES):
		label["error"] = {"code": "invalid_input", "message": "`properties` is an array of at most %d names" % MAX_BATCH_PROPERTIES}
		return label
	var listed: Dictionary
	if target["singleton"]:
		var allowed: Dictionary = SINGLETON_PROPERTIES[target["name"]]
		listed = {"names": [], "missing": [], "truncated": false}
		for name in (wanted if wanted != null else allowed.keys()):
			if allowed.has(str(name)):
				listed["names"].append(str(name))
			else:
				listed["missing"].append(str(name))
	else:
		listed = _property_names(target["object"], wanted, MAX_BATCH_PROPERTIES)
	var encoded: Dictionary = _encode_properties(target["object"], listed["names"])
	label["class"] = target["class"]
	if not target["singleton"]:
		label["node"] = target["name"]
	label["properties"] = encoded["properties"]
	label["unrepresentable"] = encoded["unrepresentable"]
	label["missing"] = listed["missing"]
	label["truncated"] = listed["truncated"]
	return label


## `runtime.properties.batch`: the properties of up to MAX_BATCH_TARGETS
## nodes or allowed singletons in one answer. A target that cannot be read
## says why; the others are read all the same.
func _properties_batch(payload: Dictionary) -> Dictionary:
	var targets = payload.get("targets")
	if typeof(targets) != TYPE_ARRAY or targets.is_empty() or targets.size() > MAX_BATCH_TARGETS:
		return fail("invalid_input", "`targets` is an array of 1 to %d targets" % MAX_BATCH_TARGETS)
	var answers: Array = []
	var spent := 0
	var truncated := false
	for item in targets:
		if spent > MAX_ANSWER_BYTES:
			truncated = true
			var skipped := {"skipped": "the answer reached its size bound; read this target on its own"}
			if typeof(item) == TYPE_DICTIONARY:
				for key in ["node", "singleton"]:
					if item.has(key):
						skipped[key] = str(item[key])
			answers.append(skipped)
			continue
		var answer: Dictionary = _read_target(item)
		spent += _json_size(answer)
		answers.append(answer)
	return ok(_observed({"targets": answers, "truncated": truncated}))


func _native_property(class_name_: String, property: String) -> bool:
	if not _native_properties.has(class_name_):
		var names := {}
		for info in ClassDB.class_get_property_list(class_name_, false):
			names[str(info["name"])] = true
		_native_properties[class_name_] = names
	return _native_properties[class_name_].has(property)


## The declared shape of `property` on `target` and whether a write may change
## it: {"info"} or {"error": refusal}.
func _writable(target: Dictionary, property: String) -> Dictionary:
	var object: Object = target["object"]
	if target["singleton"]:
		var allowed: Dictionary = SINGLETON_PROPERTIES[target["name"]]
		if not allowed.has(property):
			return {"error": fail("invalid_input", "only %s of %s are writable here" % [
				", ".join(allowed.keys()), target["name"]])}
		var rule: Array = allowed[property]
		return {"info": {"name": property, "type": rule[0], "class_name": "", "usage": 0}, "range": [rule[1], rule[2]]}
	if property == "script":
		return {"error": fail("invalid_input", "attaching or detaching a script is not a property write")}
	var info: Dictionary = {}
	for entry in object.get_property_list():
		if str(entry["name"]) == property and not (int(entry.get("usage", 0)) & SKIPPED_USAGE):
			info = entry
			break
	if info.is_empty():
		return {"error": fail("not_found", "%s has no property %s" % [target["name"], property])}
	var native_class: String = object.get_class()
	if _native_property(native_class, property) \
			and String(ClassDB.class_get_property_setter(native_class, property)) == "":
		return {"error": fail("invalid_input", "%s.%s is read-only: %s has no setter for it" % [
			target["name"], property, native_class])}
	return {"info": info}


static func _tagged_or_null(value):
	var encoded: Dictionary = VariantCodec.encode(value)
	return encoded["value"] if encoded.get("ok", false) else null


## `runtime.properties.set`: write the `properties` of one node, or of an
## allowed singleton. Every value is decoded and converted to its property's
## declared type without loss first; any that cannot be refuses the whole
## request before anything is written. Each write is read back: `matched`
## says whether the property then holds exactly the requested value, which a
## setter that clamps or refuses changes.
func _properties_set(payload: Dictionary) -> Dictionary:
	var target: Dictionary = _target_of(payload)
	if target.has("error"):
		return target["error"]
	var wanted = payload.get("properties")
	if typeof(wanted) != TYPE_DICTIONARY or wanted.is_empty() or wanted.size() > MAX_SET_PROPERTIES:
		return fail("invalid_input", "`properties` maps 1 to %d property names to tagged values" % MAX_SET_PROPERTIES)
	var object: Object = target["object"]
	var plan: Array = []
	for name in wanted.keys():
		var property := str(name)
		var checked: Dictionary = _writable(target, property)
		if checked.has("error"):
			return checked["error"]
		var decoded: Dictionary = VariantCodec.decode(wanted[name])
		if not decoded.get("ok", false):
			return fail("invalid_input", "%s: %s" % [property, str(decoded.get("reason", "not a tagged value"))])
		var converted: Dictionary = Values.convert_for(checked["info"], object.get(property), decoded["value"])
		if not converted["ok"]:
			return fail("invalid_input", "%s.%s: %s" % [target["name"], property, converted["reason"]])
		if checked.has("range"):
			var bounds: Array = checked["range"]
			var number := float(converted["value"])
			if number < bounds[0] or number > bounds[1]:
				return fail("invalid_input", "%s.%s takes %s to %s" % [target["name"], property, str(bounds[0]), str(bounds[1])])
		plan.append({"property": property, "value": converted["value"]})
	var results: Array = []
	var all_matched := true
	for step in plan:
		var property: String = step["property"]
		var before = object.get(property)
		object.set(property, step["value"])
		var after = object.get(property) if is_instance_valid(object) else null
		var matched: bool = is_instance_valid(object) and Values.same(after, step["value"])
		all_matched = all_matched and matched
		var result := {
			"property": property,
			"before": _tagged_or_null(before),
			"requested": _tagged_or_null(step["value"]),
			"after": _tagged_or_null(after),
			"matched": matched,
		}
		if not matched:
			result["note"] = "the property holds another value after the write: its setter changed or refused the value"
		results.append(result)
	var answer := {"target": target["name"], "class": target["class"], "results": results, "all_matched": all_matched}
	if not target["singleton"]:
		answer["node"] = target["name"]
	return ok(_observed(answer), "committed")


func _on_emitted(key: String) -> void:
	_emissions[key] = int(_emissions.get(key, 0)) + 1


## `callable`, taking and dropping the `count` arguments a signal passes.
## Callable.unbind refuses a count of 0.
static func _dropping(callable: Callable, count: int) -> Callable:
	return callable.unbind(count) if count > 0 else callable


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
			node.connect(name, _dropping(_on_emitted.bind(key), arguments.size()))
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


## The id of every monitor of this build's Performance.Monitor enum, by name.
static func _builtin_monitors() -> Dictionary:
	var builtin := {}
	for name in ClassDB.class_get_enum_constants("Performance", "Monitor"):
		if name != "MONITOR_MAX":
			builtin[name] = ClassDB.class_get_integer_constant("Performance", name)
	return builtin


static func _custom_monitor_names() -> Array:
	var custom_names: Array = []
	for name in Performance.get_custom_monitor_names():
		custom_names.append(str(name))
	return custom_names


## `runtime.performance`: every monitor of this build's Performance.Monitor
## enum by its name there, and the project's custom monitors, or only the
## named `monitors`. A value that is not a finite number is answered as null.
func _performance(payload: Dictionary) -> Dictionary:
	var builtin := _builtin_monitors()
	var custom_names := _custom_monitor_names()
	var wanted = payload.get("monitors")
	if wanted != null:
		for name in wanted:
			if not builtin.has(str(name)) and not custom_names.has(str(name)):
				return fail("invalid_input", "%s is not a performance monitor of this build or project" % str(name))
	var monitors := {}
	for name in builtin:
		if wanted == null or wanted.has(name):
			monitors[name] = _finite(Performance.get_monitor(builtin[name]))
	var custom := {}
	for name in custom_names:
		if wanted == null or wanted.has(name):
			custom[name] = _finite(Performance.get_custom_monitor(StringName(name)))
	return ok(_observed({"monitors": monitors, "custom": custom}))


static func _finite(value):
	if (value is float or value is int) and is_finite(float(value)):
		return value
	return null


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
		var shown = _control_text(control)
		if shown != null:
			entry["text"] = shown
		if control is BaseButton:
			entry["disabled"] = (control as BaseButton).disabled
			entry["pressed"] = (control as BaseButton).button_pressed
		controls.append(entry)
	return ok(_observed({
		"focus": str(focus.get_path()) if focus != null else null,
		"controls": controls,
		"truncated": truncated,
	}))


## Every visible Control of the game whose text is `text` (exactly, or
## containing it), in tree order, at most `limit`.
func _controls_with_text(text: String, contains: bool, limit: int, buttons_only: bool) -> Array:
	var found: Array = []
	var pending: Array = [get_tree().root]
	var cursor := 0
	while cursor < pending.size() and cursor < MAX_SCANNED:
		var node: Node = pending[cursor]
		cursor += 1
		if _own(node):
			continue
		for child in node.get_children():
			pending.append(child)
		if not (node is Control) or (buttons_only and not (node is BaseButton)):
			continue
		var control := node as Control
		if not control.is_visible_in_tree():
			continue
		var shown: Array = []
		if "text" in control:
			shown.append(str(control.get("text")))
		if control.has_method("get_parsed_text"):
			shown.append(str(control.call("get_parsed_text")))
		for candidate in shown:
			if (contains and candidate.contains(text)) or (not contains and candidate == text):
				found.append(control)
				break
		if found.size() >= limit:
			break
	return found


# --- searches and navigation ---------------------------------------------------


static func _point(value):
	if typeof(value) != TYPE_DICTIONARY or not value.has("x") or not value.has("y"):
		return null
	for key in value.keys():
		if not str(key) in ["x", "y", "z"]:
			return null
	var parts: Array = []
	for key in ["x", "y", "z"]:
		if not value.has(key):
			continue
		var part = value[key]
		if (typeof(part) != TYPE_INT and typeof(part) != TYPE_FLOAT) or not is_finite(float(part)) \
				or absf(float(part)) > MAX_COORDINATE:
			return null
		parts.append(float(part))
	if parts.size() == 3:
		return Vector3(parts[0], parts[1], parts[2])
	return Vector2(parts[0], parts[1])


static func _spelled(point) -> Dictionary:
	if point is Vector3:
		return {"x": point.x, "y": point.y, "z": point.z}
	return {"x": point.x, "y": point.y}


## Where `node` is, globally: a Vector2 for a Node2D, a Vector3 for a Node3D,
## null for anything else.
static func _global_position(node: Node):
	if node is Node2D:
		return (node as Node2D).global_position
	if node is Node3D:
		return (node as Node3D).global_position
	return null


func _class_known(name: String) -> bool:
	if ClassDB.class_exists(name):
		return true
	for entry in ProjectSettings.get_global_class_list():
		if str(entry.get("class", "")) == name:
			return true
	return false


static func _script_is(node: Node, path: String) -> bool:
	var script = node.get_script()
	while script != null:
		if str(script.resource_path) == path:
			return true
		script = script.get_base_script()
	return false


## `runtime.nodes.find`: the game's nodes under `root` matching every given
## filter — `class` (a native or script class, inherited ones included),
## `script` (attached, or extended by the attached one), `group`, `name` (a
## glob with * and ?) and `near` (within `radius` of a `point` or of another
## `node`, in 2D or 3D) — nearest first when `near` is given, tree order
## otherwise, at most `limit`.
func _find(payload: Dictionary) -> Dictionary:
	var start: Node = get_tree().root
	if str(payload.get("root", "")) not in ["", "/root"]:
		start = _resolve(payload["root"])
		if start == null:
			return fail("not_found", "%s names no node of the running game" % str(payload["root"]))
	var limit: int = clampi(int(payload.get("limit", 100)), 1, MAX_FOUND)
	var class_filter := str(payload.get("class", ""))
	var class_known := class_filter == "" or _class_known(class_filter)
	var script_filter := str(payload.get("script", ""))
	if script_filter != "" and not script_filter.begins_with("res://"):
		script_filter = "res://" + script_filter
	var group_filter := str(payload.get("group", ""))
	var name_filter := str(payload.get("name", ""))
	var near = payload.get("near")
	var centre = null
	var radius := 0.0
	var reference: Node = null
	if near != null:
		if typeof(near) != TYPE_DICTIONARY or near.has("point") == near.has("node") or not near.has("radius"):
			return fail("invalid_input", "`near` is {point or node, radius}")
		var reach = near["radius"]
		if (typeof(reach) != TYPE_INT and typeof(reach) != TYPE_FLOAT) or not is_finite(float(reach)) or float(reach) < 0.0:
			return fail("invalid_input", "`near.radius` is a non-negative number")
		radius = float(reach)
		if near.has("point"):
			centre = _point(near["point"])
			if centre == null:
				return fail("invalid_input", "`near.point` is {x, y} or {x, y, z}")
		else:
			reference = _resolve(near["node"])
			if reference == null:
				return fail("not_found", "%s names no node of the running game" % str(near["node"]))
			centre = _global_position(reference)
			if centre == null:
				return fail("invalid_input", "%s is neither a Node2D nor a Node3D, so it has no position" % str(reference.get_path()))
	var matches: Array = []
	var scanned := 0
	var scan_truncated := false
	var pending: Array = [start]
	var cursor := 0
	while cursor < pending.size():
		var node: Node = pending[cursor]
		cursor += 1
		if _own(node):
			continue
		if scanned >= MAX_SCANNED:
			scan_truncated = true
			break
		scanned += 1
		for child in node.get_children():
			pending.append(child)
		if node == reference:
			continue
		if class_filter != "" and not (node.is_class(class_filter) or Values._script_class_is(node, class_filter)):
			continue
		if script_filter != "" and not _script_is(node, script_filter):
			continue
		if group_filter != "" and not node.is_in_group(group_filter):
			continue
		if name_filter != "" and not String(node.name).match(name_filter):
			continue
		var at = _global_position(node)
		var distance := -1.0
		if centre != null:
			if at == null or typeof(at) != typeof(centre):
				continue
			distance = at.distance_to(centre)
			if distance > radius:
				continue
		matches.append([node, at, distance])
	if centre != null:
		matches.sort_custom(func(a, b): return a[2] < b[2])
	var nodes: Array = []
	for found in matches.slice(0, limit):
		var entry := _describe_node(found[0])
		if found[1] != null:
			entry["position"] = _spelled(found[1])
		if centre != null:
			entry["distance"] = found[2]
		nodes.append(entry)
	var answer := {
		"nodes": nodes,
		"matched": matches.size(),
		"truncated": matches.size() > limit,
		"scanned": scanned,
		"scan_truncated": scan_truncated,
	}
	if class_filter != "":
		answer["class_known"] = class_known
		if not class_known:
			answer["note"] = "%s is neither an engine class nor in the project's global script class list; only nodes whose script declares it with class_name match" % class_filter
	return ok(_observed(answer))


## Where `node` path-finds: {"mover", "dimension", "server", "map", "layers",
## "source"} — the node itself and its world's default navigation map, or the
## map and layers of a NavigationAgent2D/3D it has as a child (or is) — or
## {"error": refusal}.
func _navigation_of(node: Node, layers) -> Dictionary:
	var mover: Node = node
	var agent: Node = null
	if node is NavigationAgent2D or node is NavigationAgent3D:
		agent = node
		mover = node.get_parent()
	else:
		for child in node.get_children():
			if child is NavigationAgent2D or child is NavigationAgent3D:
				agent = child
				break
	var context := {"mover": mover}
	if mover is Node2D:
		context["dimension"] = 2
		context["server"] = NavigationServer2D
		context["map"] = (mover as Node2D).get_world_2d().navigation_map
	elif mover is Node3D:
		context["dimension"] = 3
		context["server"] = NavigationServer3D
		context["map"] = (mover as Node3D).get_world_3d().navigation_map
	else:
		return {"error": fail("invalid_input", "%s is neither a Node2D nor a Node3D, so it has no navigation map" % str(node.get_path()))}
	context["source"] = "world"
	context["layers"] = 1
	if agent != null and ((agent is NavigationAgent2D and context["dimension"] == 2) or (agent is NavigationAgent3D and context["dimension"] == 3)):
		context["map"] = agent.get_navigation_map()
		context["layers"] = int(agent.navigation_layers)
		context["source"] = "agent %s" % str(agent.get_path())
	if layers != null:
		context["layers"] = int(layers)
	return context


## Whether this build reports a region's own iteration on `server`.
func _region_iteration(server: Object) -> bool:
	return _compat["region_iteration_2d"] if server == NavigationServer2D else _compat["region_iteration_3d"]


## The regions' iteration ids, or [] when the build has no such API.
func _region_iterations(server: Object, regions: Array) -> Array:
	var iterations: Array = []
	if not _region_iteration(server):
		return iterations
	for region in regions:
		iterations.append(int(server.call("region_get_iteration_id", region)))
	return iterations


## Wait, at most `limit_ms`, until navigation `map` has synchronized its
## regions: it has a region, has iterated, and (on builds that sync a region
## apart from its map) has iterated again since every region synced.
## Returns "" or the reason it did not.
func _await_synced(server: Object, map: RID, limit_ms: int, message_id: String, epoch: int) -> String:
	var started := _now_ms()
	var signature = null
	var ready_after := -1
	while true:
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			return "interrupted: %s" % interrupted
		var iteration: int = server.map_get_iteration_id(map)
		var regions: Array = server.map_get_regions(map)
		if iteration > 0 and not regions.is_empty():
			var iterations := _region_iterations(server, regions)
			if not _region_iteration(server):
				return ""
			var synced := true
			for value in iterations:
				synced = synced and value > 0
			if synced:
				if str(iterations) != str(signature):
					# The map reflects regions that changed from its next
					# iteration on; regions already synced at the first look
					# are taken as reflected.
					ready_after = iteration if signature != null else iteration - 1
					signature = iterations
				if iteration > ready_after:
					return ""
			else:
				signature = iterations
		if _now_ms() - started >= limit_ms:
			if regions.is_empty():
				return "the navigation map has no navigation region"
			if iteration == 0:
				return "the navigation map has not synchronized yet"
			return "the navigation map's regions have not synchronized yet"
		await get_tree().physics_frame
	return ""


## A path over `context`'s map from `from` to `to`: {"path", "iteration"} or
## {"error": refusal}. An empty path is asked again if the map iterates within
## the next RETRY_FRAMES physics frames: a region that has just synced is in
## the map from its next iteration on.
func _query_path(context: Dictionary, from, to, optimize: bool, sync_ms: int, message_id: String, epoch: int) -> Dictionary:
	var server: Object = context["server"]
	var map: RID = context["map"]
	var started := _now_ms()
	var reason: String = await _await_synced(server, map, sync_ms, message_id, epoch)
	if reason != "":
		if reason.begins_with("interrupted"):
			return {"error": fail("cancelled", "the path query was %s" % reason)}
		return {"error": fail("unavailable", reason)}
	var path = server.map_get_path(map, from, to, optimize, int(context["layers"]))
	if path.is_empty():
		var iteration: int = server.map_get_iteration_id(map)
		var frames := 0
		while server.map_get_iteration_id(map) == iteration and frames < RETRY_FRAMES \
				and _now_ms() - started < sync_ms and _interrupted(message_id, epoch) == "":
			await get_tree().physics_frame
			frames += 1
		path = server.map_get_path(map, from, to, optimize, int(context["layers"]))
	return {"path": path, "iteration": server.map_get_iteration_id(map)}


static func _path_length(path) -> float:
	var length := 0.0
	for index in range(1, path.size()):
		length += path[index - 1].distance_to(path[index])
	return length


## The distance within which a path's end counts as the target.
static func _tolerance(context: Dictionary) -> float:
	var server: Object = context["server"]
	var size: float = server.map_get_cell_size(context["map"])
	if context["dimension"] == 3:
		size = maxf(size, server.map_get_cell_height(context["map"]))
	return maxf(size, 0.001)


## The facts a path answer carries.
func _path_facts(context: Dictionary, path, to) -> Dictionary:
	var points: Array = []
	for point in path:
		points.append(_spelled(point))
	var end_distance = path[path.size() - 1].distance_to(to) if not path.is_empty() else null
	return {
		"dimension": context["dimension"],
		"map": context["source"],
		"layers": context["layers"],
		"path": points,
		"length": _path_length(path),
		"end_distance": end_distance,
		"reaches_target": end_distance != null and end_distance <= _tolerance(context),
	}


## `runtime.navigation.path`: a path over the navigation map `node` uses from
## `from` (its own position by default) to `to`, once that map has synced.
func _navigation_path(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var node: Node = _resolve(payload.get("node", ""))
	if node == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("node", "")))
	var context: Dictionary = _navigation_of(node, payload.get("layers"))
	if context.has("error"):
		return context["error"]
	var to = _point(payload.get("to"))
	var from = _point(payload.get("from")) if payload.has("from") else _global_position(context["mover"])
	var expected: int = TYPE_VECTOR2 if context["dimension"] == 2 else TYPE_VECTOR3
	if to == null or from == null or typeof(to) != expected or typeof(from) != expected:
		return fail("invalid_input", "`from` and `to` are %s points for %s" % [
			"{x, y}" if context["dimension"] == 2 else "{x, y, z}", str(context["mover"].get_path())])
	var sync_ms: int = clampi(int(payload.get("sync_timeout_ms", 1000)), 1, MAX_SYNC_MS)
	var queried: Dictionary = await _query_path(context, from, to, bool(payload.get("optimize", true)), sync_ms, message_id, epoch)
	_cancelled.erase(message_id)
	if queried.has("error"):
		return queried["error"]
	var facts := _path_facts(context, queried["path"], to)
	facts["node"] = str(node.get_path())
	facts["from"] = _spelled(from)
	facts["to"] = _spelled(to)
	facts["map_iteration"] = queried["iteration"]
	return ok(_observed(facts))


## `runtime.node.move`: put a Node2D or Node3D at `to` (`teleport`), or walk
## it there along its navigation map's path at `speed` units per second
## (`along_path`) by setting its global position every physics frame. Either
## way the node is moved directly: the game's own movement code and its
## physics collision response do not run for it.
func _move(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var node: Node = _resolve(payload.get("node", ""))
	if node == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("node", "")))
	var start = _global_position(node)
	if start == null:
		return fail("invalid_input", "%s is neither a Node2D nor a Node3D, so it has no position to move" % str(node.get_path()))
	var to = _point(payload.get("to"))
	if to == null or typeof(to) != typeof(start):
		return fail("invalid_input", "`to` is a %s point for %s" % ["{x, y}" if start is Vector2 else "{x, y, z}", str(node.get_path())])
	match str(payload.get("mode", "")):
		"teleport":
			node.global_position = to
			var after = _global_position(node)
			await get_tree().physics_frame
			var held: bool = is_instance_valid(node) and _global_position(node).distance_to(to) <= 0.001
			_cancelled.erase(message_id)
			return ok(_observed({
				"node": str(node.get_path()) if is_instance_valid(node) else null,
				"mode": "teleport",
				"outcome": "teleported",
				"start": _spelled(start),
				"after": _spelled(after),
				"final": _spelled(_global_position(node)) if is_instance_valid(node) else null,
				"held": held,
			}), "committed")
		"along_path":
			return await _walk(node, start, to, payload, message_id, epoch)
	return fail("invalid_input", "`mode` is teleport or along_path")


func _walk(node: Node, start, to, payload: Dictionary, message_id: String, epoch: int) -> Dictionary:
	var speed = payload.get("speed")
	if (typeof(speed) != TYPE_INT and typeof(speed) != TYPE_FLOAT) or float(speed) <= 0.0 or float(speed) > MAX_SPEED:
		return fail("invalid_input", "along_path needs `speed` in (0, %d] units per second" % int(MAX_SPEED))
	var timeout: int = clampi(int(payload.get("timeout_ms", 10000)), 1, MAX_MOVE_MS)
	var context: Dictionary = _navigation_of(node, payload.get("layers"))
	if context.has("error"):
		return context["error"]
	var sync_ms: int = clampi(int(payload.get("sync_timeout_ms", 1000)), 1, MAX_SYNC_MS)
	var queried: Dictionary = await _query_path(context, start, to, bool(payload.get("optimize", true)), sync_ms, message_id, epoch)
	if queried.has("error"):
		_cancelled.erase(message_id)
		return queried["error"]
	var path = queried["path"]
	if path.is_empty():
		_cancelled.erase(message_id)
		return fail("unavailable", "the navigation map has no path from %s to %s" % [str(_spelled(start)), str(_spelled(to))])
	var started := _now_ms()
	var waypoint := 1
	var frames := 0
	var stalled := 0
	var moved := 0.0
	var best := INF
	var outcome := "arrived"
	var position = start
	while waypoint < path.size():
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			outcome = "cancelled" if interrupted == "cancelled" else "interrupted"
			break
		if _now_ms() - started >= timeout:
			outcome = "timeout"
			break
		await get_tree().physics_frame
		if not is_instance_valid(node) or not node.is_inside_tree():
			outcome = "interrupted"
			break
		position = _global_position(node)
		var step: float = float(speed) * get_physics_process_delta_time()
		while step > 0.0 and waypoint < path.size():
			var target = path[waypoint]
			var distance: float = position.distance_to(target)
			if distance <= step:
				position = target
				step -= distance
				moved += distance
				waypoint += 1
			else:
				position = position.move_toward(target, step)
				moved += step
				step = 0.0
		node.global_position = position
		frames += 1
		var remaining: float = position.distance_to(path[mini(waypoint, path.size() - 1)])
		for index in range(waypoint, path.size() - 1):
			remaining += path[index].distance_to(path[index + 1])
		if remaining < best - 0.0001:
			best = remaining
			stalled = 0
		else:
			stalled += 1
			if stalled >= BLOCKED_FRAMES:
				outcome = "blocked"
				break
	_cancelled.erase(message_id)
	var alive := is_instance_valid(node)
	var final = _global_position(node) if alive else position
	if frames == 0:
		var reason := {"cancelled": "the move was withdrawn before the node moved",
			"interrupted": "the move ended before the node moved: the session stopped or its connection ended, or the node left the tree",
			"timeout": "the move timed out before the node moved"}
		return fail("cancelled" if outcome != "timeout" else "timeout", str(reason.get(outcome, "the node did not move")))
	var facts := _path_facts(context, path, to)
	facts["node"] = str(node.get_path()) if alive else null
	facts["mode"] = "along_path"
	facts["outcome"] = outcome
	facts["start"] = _spelled(start)
	facts["final"] = _spelled(final)
	facts["to"] = _spelled(to)
	facts["moved"] = moved
	facts["remaining"] = final.distance_to(path[path.size() - 1])
	facts["frames"] = frames
	facts["elapsed_ms"] = _now_ms() - started
	facts["speed"] = float(speed)
	return ok(_observed(facts), "committed")


# --- waits and samples -----------------------------------------------------------


## One condition a wait evaluates: {"kind", "check": Callable answering
## {"satisfied", "observed"} or {"error"}, "cleanup": Callable} or {"error"}.
func _condition(condition) -> Dictionary:
	if typeof(condition) != TYPE_DICTIONARY or condition.size() != 1:
		return {"error": fail("invalid_input", "`condition` is an object with one member, keyed by its kind")}
	var kind: String = str(condition.keys()[0])
	var fields = condition[kind]
	if typeof(fields) != TYPE_DICTIONARY:
		return {"error": fail("invalid_input", "a %s condition takes an object of its fields" % kind)}
	var nothing := func() -> void: pass
	match kind:
		"node_exists", "node_gone":
			var spelled := str(fields.get("node", ""))
			var gone := kind == "node_gone"
			var check := func() -> Dictionary:
				var found: Node = _resolve(spelled)
				return {"satisfied": (found == null) == gone, "observed": {"present": found != null, "node": str(found.get_path()) if found != null else null}}
			return {"kind": kind, "check": check, "cleanup": nothing}
		"property":
			var spelled := str(fields.get("node", ""))
			var property := str(fields.get("property", ""))
			var op := str(fields.get("op", "=="))
			if not Values.COMPARISONS.has(op):
				return {"error": fail("invalid_input", "`op` is one of %s" % ", ".join(Values.COMPARISONS))}
			var decoded: Dictionary = VariantCodec.decode(fields.get("value"))
			if not decoded.get("ok", false):
				return {"error": fail("invalid_input", "`value`: %s" % str(decoded.get("reason", "not a tagged value")))}
			var expected = decoded["value"]
			var tolerance := float(fields.get("tolerance", 0.0))
			var check := func() -> Dictionary:
				var found: Node = _resolve(spelled)
				if found == null:
					return {"satisfied": false, "observed": {"present": false}}
				var declared := TYPE_NIL
				var listed := false
				for info in found.get_property_list():
					if str(info["name"]) == property:
						listed = true
						declared = int(info.get("type", TYPE_NIL))
						break
				if not listed:
					return {"error": fail("not_found", "%s has no property %s" % [str(found.get_path()), property])}
				var live = found.get(property)
				var compared: Dictionary = Values.compare(live, op, expected, tolerance)
				var observed := {"present": true, "value": _tagged_or_null(live)}
				if not compared["ok"]:
					if declared != TYPE_NIL:
						return {"error": fail("invalid_input", "%s.%s: %s" % [str(found.get_path()), property, compared["reason"]])}
					observed["comparable"] = false
					return {"satisfied": false, "observed": observed}
				return {"satisfied": compared["value"], "observed": observed}
			return {"kind": kind, "check": check, "cleanup": nothing}
		"signal":
			var node: Node = _resolve(fields.get("node", ""))
			if node == null:
				return {"error": fail("not_found", "%s names no node of the running game" % str(fields.get("node", "")))}
			var name := str(fields.get("signal", ""))
			var count: int = clampi(int(fields.get("count", 1)), 1, MAX_WAIT_SIGNAL_COUNT)
			var arguments := -1
			for info in node.get_signal_list():
				if str(info["name"]) == name:
					arguments = info.get("args", []).size()
					break
			if arguments < 0:
				return {"error": fail("not_found", "%s has no signal %s" % [str(node.get_path()), name])}
			var counter := {"n": 0}
			var counting := _dropping(func() -> void: counter["n"] += 1, arguments)
			node.connect(name, counting)
			var check := func() -> Dictionary:
				return {"satisfied": int(counter["n"]) >= count, "observed": {"emissions": counter["n"], "present": is_instance_valid(node)}}
			var cleanup := func() -> void:
				if is_instance_valid(node) and node.is_connected(name, counting):
					node.disconnect(name, counting)
			return {"kind": kind, "check": check, "cleanup": cleanup}
		"ui_text":
			var text := str(fields.get("text", ""))
			var contains := str(fields.get("match", "exact")) == "contains"
			var check := func() -> Dictionary:
				var found: Array = _controls_with_text(text, contains, 1, false)
				if found.is_empty():
					return {"satisfied": false, "observed": {"present": false}}
				var control: Control = found[0]
				return {"satisfied": true, "observed": {"present": true, "control": str(control.get_path()), "text": str(control.get("text")) if "text" in control else null}}
			return {"kind": kind, "check": check, "cleanup": nothing}
	return {"error": fail("invalid_input", "%s is not a condition kind" % kind)}


## `runtime.wait`: evaluate `condition` every frame until it holds or
## `timeout_ms` passes. A withdrawal, the session stopping and the connection
## ending all end the wait at its next frame.
func _wait(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var timeout: int = int(payload.get("timeout_ms", 5000))
	if timeout < 1 or timeout > MAX_WAIT_MS:
		return fail("invalid_input", "a wait lasts 1 to %d ms" % MAX_WAIT_MS)
	var condition: Dictionary = _condition(payload.get("condition"))
	if condition.has("error"):
		return condition["error"]
	var started := _now_ms()
	var first_frame := Engine.get_process_frames()
	var outcome := "timeout"
	var last: Dictionary = {}
	while true:
		last = condition["check"].call()
		if last.has("error"):
			condition["cleanup"].call()
			_cancelled.erase(message_id)
			return last["error"]
		if last.get("satisfied", false):
			outcome = "satisfied"
			break
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			outcome = "cancelled" if interrupted == "cancelled" else "interrupted"
			break
		if _now_ms() - started >= timeout:
			break
		await get_tree().process_frame
	condition["cleanup"].call()
	_cancelled.erase(message_id)
	return ok(_observed({
		"condition": condition["kind"],
		"satisfied": outcome == "satisfied",
		"outcome": outcome,
		"elapsed_ms": _now_ms() - started,
		"frames_waited": Engine.get_process_frames() - first_frame,
		"timeout_ms": timeout,
		"observed": last.get("observed"),
	}))


## The series a sample request names, in its order, each {"kind", "read":
## Callable, "present": Callable} with what names it: {"series"} or {"error":
## refusal}. A series is a `property` of a `node` or of an allowed `singleton`,
## or a performance `monitor`.
func _series_of(payload: Dictionary) -> Dictionary:
	var requested = payload.get("series")
	if typeof(requested) != TYPE_ARRAY or requested.is_empty() or requested.size() > MAX_SERIES:
		return {"error": fail("invalid_input", "`series` is an array of 1 to %d series" % MAX_SERIES)}
	var builtin := _builtin_monitors()
	var custom_names := _custom_monitor_names()
	var series: Array = []
	for item in requested:
		if typeof(item) != TYPE_DICTIONARY:
			return {"error": fail("invalid_input", "a series is an object")}
		if item.has("monitor"):
			var monitor := str(item["monitor"])
			if builtin.has(monitor):
				var id: int = builtin[monitor]
				series.append({"kind": "monitor", "monitor": monitor, "custom": false,
					"present": func() -> bool: return true,
					"read": func(): return _finite(Performance.get_monitor(id))})
			elif custom_names.has(monitor):
				series.append({"kind": "monitor", "monitor": monitor, "custom": true,
					"present": func() -> bool: return true,
					"read": func(): return _finite(Performance.get_custom_monitor(StringName(monitor)))})
			else:
				return {"error": fail("invalid_input", "%s is not a performance monitor of this build or project" % monitor)}
			continue
		var target: Dictionary = _target_of(item)
		if target.has("error"):
			return target
		var property := str(item.get("property", ""))
		if target["singleton"]:
			if not SINGLETON_PROPERTIES[target["name"]].has(property):
				return {"error": fail("invalid_input", "only %s of %s are sampled" % [
					", ".join(SINGLETON_PROPERTIES[target["name"]].keys()), target["name"]])}
		elif not _property_names(target["object"], [property], 1)["missing"].is_empty():
			return {"error": fail("not_found", "%s has no property %s" % [target["name"], property])}
		var object: Object = target["object"]
		var singleton: bool = target["singleton"]
		var entry := {"kind": "property", "property": property}
		entry["singleton" if singleton else "node"] = target["name"]
		entry["present"] = func() -> bool: return is_instance_valid(object) and (singleton or (object as Node).is_inside_tree())
		entry["read"] = func(): return object.get(property) if is_instance_valid(object) else null
		series.append(entry)
	return {"series": series}


## `runtime.sample`: read every named series at most every `interval_ms`, for
## `duration_ms` or `max_samples` samples, and answer each series' values in
## the order they were asked for, with the frame and time base of each sample,
## whether the game was paused or its window unfocused then, and the minimum,
## maximum, mean and last of each numeric series.
func _sample(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var duration: int = int(payload.get("duration_ms", 1000))
	var interval: int = int(payload.get("interval_ms", 100))
	var most: int = int(payload.get("max_samples", MAX_SAMPLES))
	if duration < 1 or duration > MAX_SAMPLE_MS or interval < 1 or interval > MAX_SAMPLE_INTERVAL_MS \
			or most < 1 or most > MAX_SAMPLES:
		return fail("invalid_input", "a sample lasts 1 to %d ms, every 1 to %d ms, for at most %d samples" % [
			MAX_SAMPLE_MS, MAX_SAMPLE_INTERVAL_MS, MAX_SAMPLES])
	var resolved: Dictionary = _series_of(payload)
	if resolved.has("error"):
		return resolved["error"]
	var series: Array = resolved["series"]
	var columns: Array = []
	var raw: Array = []
	var unsampled: Array = []
	var absent: Array = []
	for _entry in series:
		columns.append([])
		raw.append([])
		unsampled.append({})
		absent.append(0)
	var samples: Array = []
	var started := _now_ms()
	var first_frame := Engine.get_process_frames()
	var first_physics := Engine.get_physics_frames()
	var next_at := started
	var spent := 0
	var stopped_by := "duration"
	var paused := 0
	var unfocused := 0
	while true:
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			stopped_by = "cancelled" if interrupted == "cancelled" else "interrupted"
			break
		var now := _now_ms()
		if now - started > duration:
			break
		if now >= next_at:
			var is_paused: bool = get_tree().paused
			var focused: bool = get_tree().root.has_focus()
			paused += 1 if is_paused else 0
			unfocused += 0 if focused else 1
			samples.append({
				"t_ms": now - started,
				"frame": Engine.get_process_frames(),
				"physics_frame": Engine.get_physics_frames(),
				"paused": is_paused,
				"focused": focused,
				"time_scale": Engine.time_scale,
			})
			spent += 96
			for index in series.size():
				var entry: Dictionary = series[index]
				if not entry["present"].call():
					absent[index] += 1
					columns[index].append(null)
					raw[index].append(null)
					continue
				var value = entry["read"].call()
				if entry["kind"] == "monitor":
					columns[index].append(value)
					raw[index].append(value)
					spent += 24
					continue
				var why: String = Values.unsampled(value)
				var encoded: Dictionary = VariantCodec.encode(value) if why == "" else {"ok": false, "reason": why}
				if not encoded.get("ok", false):
					unsampled[index][str(encoded.get("reason", "not representable"))] = true
					columns[index].append(null)
					raw[index].append(null)
					continue
				columns[index].append(encoded["value"])
				raw[index].append(value)
				spent += _json_size(encoded["value"])
			while next_at <= now:
				next_at += interval
			if samples.size() >= most:
				stopped_by = "max_samples"
				break
			if spent > MAX_ANSWER_BYTES:
				stopped_by = "size"
				break
		await get_tree().process_frame
	_cancelled.erase(message_id)
	var answered: Array = []
	for index in series.size():
		var entry: Dictionary = series[index]
		var answer := {"kind": entry["kind"], "values": columns[index], "stats": Values.stats(raw[index])}
		for key in ["node", "singleton", "property", "monitor", "custom"]:
			if entry.has(key):
				answer[key] = entry[key]
		if entry["kind"] == "property":
			answer["absent"] = absent[index]
			answer["unsampled"] = unsampled[index].keys()
		answered.append(answer)
	return ok(_observed({
		"time_base": {
			"clock": "engine_ticks_ms",
			"started_ticks_ms": started,
			"ended_ticks_ms": _now_ms(),
			"first_frame": first_frame,
			"last_frame": Engine.get_process_frames(),
			"first_physics_frame": first_physics,
			"last_physics_frame": Engine.get_physics_frames(),
			"interval_ms": interval,
			"duration_ms": duration,
			"display": DisplayServer.get_name(),
			"focus_observable": _compat["window_focus"],
		},
		"samples": samples,
		"series": answered,
		"paused_samples": paused,
		"unfocused_samples": unfocused,
		"stopped_by": stopped_by,
		"truncated": stopped_by == "size",
	}))


# --- synthetic input -----------------------------------------------------------


## Deliver `event` as the game's input, marked as the probe's own.
func _inject(event: InputEvent) -> void:
	event.set_meta(PROBE_META, true)
	Input.parse_input_event(event)


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
		_inject(released)
		Input.action_release((released as InputEventAction).action)
	elif released is InputEventKey:
		(released as InputEventKey).pressed = false
		(released as InputEventKey).echo = false
		_inject(released)
	elif released is InputEventMouseButton:
		(released as InputEventMouseButton).pressed = false
		(released as InputEventMouseButton).double_click = false
		(released as InputEventMouseButton).position = _mouse_position
		(released as InputEventMouseButton).global_position = _mouse_position
		_inject(released)
	elif released is InputEventJoypadButton:
		(released as InputEventJoypadButton).pressed = false
		_inject(released)
	elif released is InputEventJoypadMotion:
		(released as InputEventJoypadMotion).axis_value = 0.0
		_inject(released)


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
	_inject(motion)


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
	_inject(built["event"])
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
	if run == 0 and failure.is_empty():
		return fail("cancelled", "the sequence was withdrawn before its first step; nothing was sent")
	if not failure.is_empty():
		failure["message"] = "%s (after %d of %d steps; everything the sequence pressed was released)" % [failure["message"], run, steps.size()]
		failure["mutation_status"] = status
		return failure
	return ok(_observed({"steps_run": run, "cancelled": cancelled, "held": _held_snapshot()}), status)


# --- input recording and replay ------------------------------------------------


func _on_window_input(event: InputEvent) -> void:
	if not _recording.get("active", false):
		return
	var elapsed := _now_ms() - int(_recording["started_ms"])
	if elapsed > int(_recording["max_ms"]):
		_end_recording("max_duration")
		return
	if _recording["events"].size() >= int(_recording["max_events"]):
		_recording["dropped"] = int(_recording["dropped"]) + 1
		_end_recording("max_events")
		return
	_recording["events"].append(Events.describe(event, elapsed, event.has_meta(PROBE_META)))


## Stop capturing input into the recording, which is kept until it is
## stopped.
func _end_recording(reason: String) -> void:
	if not _recording.get("active", false):
		return
	_recording["active"] = false
	_recording["ended_by"] = reason
	_recording["ended_ms"] = _now_ms()
	var root := get_tree().root
	if root.window_input.is_connected(_on_window_input):
		root.window_input.disconnect(_on_window_input)


## `runtime.input.record.start`: record every input event the game's root
## window receives — keys, mouse buttons and motion, joypad buttons and axes,
## actions — with its time from now, at most `max_events` of them for at most
## `max_duration_ms`.
func _record_start(payload: Dictionary) -> Dictionary:
	if not _recording.is_empty():
		return fail("conflict", "an input recording is already kept; stop it with input.recording.stop first")
	var most: int = int(payload.get("max_events", MAX_INPUT_EVENTS))
	var longest: int = int(payload.get("max_duration_ms", MAX_INPUT_RECORDING_MS))
	if most < 1 or most > MAX_INPUT_EVENTS or longest < 1 or longest > MAX_INPUT_RECORDING_MS:
		return fail("invalid_input", "a recording keeps 1 to %d events for 1 to %d ms" % [MAX_INPUT_EVENTS, MAX_INPUT_RECORDING_MS])
	_recording = {
		"id": "input-recording-%d" % _next_input_recording,
		"started_ms": _now_ms(),
		"max_events": most,
		"max_ms": longest,
		"events": [],
		"dropped": 0,
		"ended_by": "",
		"active": true,
	}
	_next_input_recording += 1
	get_tree().root.window_input.connect(_on_window_input)
	return ok(_observed({
		"recording_id": _recording["id"],
		"started_ticks_ms": _recording["started_ms"],
		"max_events": most,
		"max_duration_ms": longest,
	}), "committed")


## `runtime.input.record.stop`: end the recording and answer every event it
## kept.
func _record_stop() -> Dictionary:
	if _recording.is_empty():
		return fail("not_found", "no input recording is kept; start one with input.recording.start")
	_end_recording("stopped")
	var events: Array = _recording["events"]
	var from_probe := 0
	for event in events:
		if str(event.get("source", "")) == "probe":
			from_probe += 1
	var answer := {
		"recording_id": _recording["id"],
		"started_ticks_ms": _recording["started_ms"],
		"duration_ms": int(_recording["ended_ms"]) - int(_recording["started_ms"]),
		"ended_by": _recording["ended_by"],
		"max_events": _recording["max_events"],
		"max_duration_ms": _recording["max_ms"],
		"dropped": _recording["dropped"],
		"events": events,
		"counts": {"total": events.size(), "probe": from_probe, "external": events.size() - from_probe},
	}
	_recording = {}
	return ok(_observed(answer), "committed")


## `runtime.input.replay`: deliver recorded `events` again at their recorded
## times from now. Everything the replay pressed and still holds is released
## when it ends, is withdrawn or the session stops, and when the connection
## ends.
func _replay(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var entries = payload.get("events")
	if typeof(entries) != TYPE_ARRAY or entries.is_empty() or entries.size() > MAX_INPUT_EVENTS:
		return fail("invalid_input", "`events` is an array of 1 to %d recorded events" % MAX_INPUT_EVENTS)
	var plan: Array = []
	var skipped := 0
	var last := 0
	for index in entries.size():
		var entry = entries[index]
		if typeof(entry) != TYPE_DICTIONARY:
			return fail("invalid_input", "event %d is not an object" % index)
		var at = entry.get("t_ms")
		if (typeof(at) != TYPE_INT and typeof(at) != TYPE_FLOAT) or float(at) != floor(float(at)) \
				or float(at) < last or float(at) > MAX_REPLAY_MS:
			return fail("invalid_input", "event %d: `t_ms` is a whole number from the previous event's to %d" % [index, MAX_REPLAY_MS])
		last = int(at)
		var built: Dictionary = Events.build(entry, index)
		if built.has("error"):
			return fail("invalid_input", built["error"])
		if built.has("skip"):
			skipped += 1
			continue
		plan.append({"t_ms": int(at), "event": built["event"], "key": built["key"]})
	if plan.is_empty():
		return fail("invalid_input", "none of the %d events is of a kind a replay delivers" % entries.size())
	var before: Dictionary = _held.duplicate()
	var started := _now_ms()
	var sent := 0
	var lag := 0
	var outcome := "completed"
	var cursor := 0
	while cursor < plan.size():
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			outcome = "cancelled" if interrupted == "cancelled" else "interrupted"
			break
		var now := _now_ms() - started
		while cursor < plan.size() and int(plan[cursor]["t_ms"]) <= now:
			var step: Dictionary = plan[cursor]
			var event: InputEvent = step["event"]
			var key: String = step["key"]
			if event is InputEventMouse:
				_mouse_position = (event as InputEventMouse).position
			if key != "":
				if Events.presses(event):
					# Held until the recording releases it or the replay ends.
					_held[key] = {"event": event, "expires_ms": _now_ms() + MAX_REPLAY_MS + MAX_HOLD_MS}
				else:
					_held.erase(key)
			_inject(event)
			lag = maxi(lag, now - int(step["t_ms"]))
			sent += 1
			cursor += 1
		if cursor < plan.size():
			await get_tree().process_frame
	_cancelled.erase(message_id)
	var released: Array = []
	for key in _held.keys():
		if not before.has(key):
			released.append(key)
			_release(key)
	released.sort()
	if sent > 0 and _interrupted(message_id, epoch) in ["", "cancelled"]:
		# Input is delivered to the game at its next frame: the answer comes
		# once the game has received every event the replay sent.
		await get_tree().process_frame
	if sent == 0:
		return fail("cancelled", "the replay ended before it sent an event: it was %s" % outcome)
	return ok(_observed({
		"outcome": outcome,
		"events_sent": sent,
		"events_skipped": skipped,
		"events_planned": plan.size(),
		"elapsed_ms": _now_ms() - started,
		"scheduled_ms": last,
		"max_lag_ms": lag,
		"released": released,
		"held": _held_snapshot(),
	}), "committed")


# --- clicking a button -------------------------------------------------------------


## The button one press names, by `node` path or by visible `text`:
## {"button"} or {"error": refusal}.
func _press_target(payload: Dictionary) -> Dictionary:
	if payload.has("node") == payload.has("text"):
		return {"error": fail("invalid_input", "name the button by either `node` or `text`")}
	if payload.has("node"):
		var node: Node = _resolve(payload["node"])
		if node == null:
			return {"error": fail("not_found", "%s names no node of the running game" % str(payload["node"]))}
		if not (node is BaseButton):
			return {"error": fail("invalid_input", "%s is a %s, not a button (BaseButton)" % [str(node.get_path()), node.get_class()])}
		return {"button": node}
	var text := str(payload["text"])
	var contains := str(payload.get("match", "exact")) == "contains"
	var found: Array = _controls_with_text(text, contains, 8, true)
	if found.is_empty():
		return {"error": fail("not_found", "no visible button shows the text %s" % text)}
	if found.size() > 1:
		var paths: Array = []
		for button in found:
			paths.append(str(button.get_path()))
		return {"error": fail("conflict", "%d visible buttons show the text %s: %s; name one by `node`" % [found.size(), text, ", ".join(paths)])}
	return {"button": found[0]}


## `runtime.ui.press`: click a visible, enabled button at the centre of its
## rectangle with the left mouse button, through the same synthetic input the
## mouse tool sends, and report whether its `pressed` signal fired and how the
## button is afterwards.
func _press(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var target: Dictionary = _press_target(payload)
	if target.has("error"):
		return target["error"]
	var button: BaseButton = target["button"]
	var path := str(button.get_path())
	if button.disabled:
		return fail("invalid_input", "%s is disabled" % path)
	if not button.is_visible_in_tree():
		return fail("invalid_input", "%s is not visible" % path)
	if button.mouse_filter == Control.MOUSE_FILTER_IGNORE:
		return fail("invalid_input", "%s ignores the mouse (mouse_filter is MOUSE_FILTER_IGNORE)" % path)
	if not (button.button_mask & MOUSE_BUTTON_MASK_LEFT):
		return fail("invalid_input", "%s does not take the left mouse button (button_mask)" % path)
	var root: Window = get_tree().root
	if button.get_viewport() != root:
		return fail("unsupported", "%s is not in the game's root viewport; only a button there is clicked" % path)
	if button.size.x <= 0.0 or button.size.y <= 0.0:
		return fail("invalid_input", "%s has no area to click" % path)
	var centre: Vector2 = root.get_final_transform() * (button.get_global_transform_with_canvas() * (button.size / 2.0))
	if not Rect2(Vector2.ZERO, Vector2(root.size)).has_point(centre):
		var headless := " (a headless session's root viewport is 64x64 pixels; start the session with display window)" \
			if DisplayServer.get_name() == "headless" else ""
		return fail("unavailable", "%s's centre (%s, %s) lies outside the game's %dx%d root viewport%s" % [
			path, str(centre.x), str(centre.y), root.size.x, root.size.y, headless])
	var hold: int = clampi(int(payload.get("hold_ms", DEFAULT_PRESS_HOLD_MS)), 1, MAX_PRESS_HOLD_MS)
	var counts := {"pressed": 0, "button_down": 0, "button_up": 0, "toggled": 0}
	var listeners := {}
	for name in counts.keys():
		var arguments: int = 1 if name == "toggled" else 0
		listeners[name] = _dropping(func() -> void: counts[name] += 1, arguments)
		button.connect(name, listeners[name])
	_move_mouse(centre)
	await get_tree().process_frame
	var press := InputEventMouseButton.new()
	press.button_index = MOUSE_BUTTON_LEFT
	press.button_mask = MOUSE_BUTTON_MASK_LEFT
	press.pressed = true
	press.position = centre
	press.global_position = centre
	if _held.has("mouse:left"):
		_release("mouse:left")
	_inject(press)
	_held["mouse:left"] = {"event": press, "expires_ms": _now_ms() + hold + MAX_HOLD_MS}
	var pressed_at := _now_ms()
	await get_tree().process_frame
	var outcome := "clicked"
	while _now_ms() - pressed_at < hold:
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			outcome = "cancelled" if interrupted == "cancelled" else "interrupted"
			break
		await get_tree().process_frame
	_release("mouse:left")
	for _frame in 2:
		await get_tree().process_frame
	_cancelled.erase(message_id)
	var alive := is_instance_valid(button)
	if alive:
		for name in listeners:
			if button.is_connected(name, listeners[name]):
				button.disconnect(name, listeners[name])
	return ok(_observed({
		"node": path,
		"text": str(button.get("text")) if alive and "text" in button else null,
		"position": {"x": centre.x, "y": centre.y},
		"outcome": outcome,
		"pressed_signal": int(counts["pressed"]) > 0,
		"emissions": counts,
		"button_pressed": button.button_pressed if alive else null,
		"toggle_mode": button.toggle_mode if alive else null,
		"disabled": button.disabled if alive else null,
		"present": alive and button.is_inside_tree(),
	}), "committed")


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


func _recording_capture(payload: Dictionary, message_id: String) -> Dictionary:
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
	_end_recording("stopped")
	_stopping = true
	_quit_soon.call_deferred(clampi(int(payload.get("exit_code", 0)), 0, 125))
	return ok(_observed({"stopping": true, "released": released}), "committed")


## `runtime.script.run`: run the caller's source and answer its report. Godot
## cannot stop the code, so a withdrawal, the session stopping or the
## connection ending answers at the next frame that the run is unresolved and
## leaves the code running; the requests behind it are served meanwhile, and
## stopping the session ends it.
func _script_run(payload: Dictionary, message_id: String) -> Dictionary:
	var offered: bool = _server_methods.has(ScriptRunner.RUNTIME_METHOD)
	if offered and _log == null:
		_log = EditorLog.new()
		_log.start()
	var epoch := _epoch
	var settled := {}
	_settle_script_run(settled, payload, offered)
	while settled.is_empty():
		var interrupted := _interrupted(message_id, epoch)
		if interrupted != "":
			_cancelled.erase(message_id)
			return fail("cancelled", "the run was withdrawn (%s) while its code still runs; it goes on until it returns or the session stops" % interrupted, "unresolved")
		await get_tree().process_frame
	_cancelled.erase(message_id)
	return settled["answer"]


func _settle_script_run(settled: Dictionary, payload: Dictionary, offered: bool) -> void:
	settled["answer"] = await _scripts.run(ScriptRunner.RUNTIME_METHOD, offered, payload, _log, self)


func _quit_soon(code: int) -> void:
	# Let the bridge send the answer before the game exits.
	for frame in 3:
		await get_tree().process_frame
	get_tree().quit(code)


# --- test scenarios: reference images, comparisons, checks and stress ----------
#
# What `runtime.image.compare` and `test.scenario.run` add to the methods
# above. The server uploads a reference image in bounded chunks
# (`runtime.image.reference`) and has it compared with a capture of the root
# viewport after the next drawn frame (`runtime.image.compare`), both decoded
# and compared with Godot's own Image API. A scenario's assertions are decided
# here from the live value (`runtime.scenario.check`): a property compared as
# runtime.wait compares, or a Control's text as runtime.ui reports it. A stress
# window (`runtime.scenario.stress`) instances a project scene, samples the
# monitors as runtime.sample does and frees every instance it added.

## The largest reference image one upload holds.
const MAX_REFERENCE_BYTES := 16777216
## How long a comparison works before it lets a frame pass, in ms.
const COMPARE_SLICE_MS := 40
## The colour of a differing pixel in a difference image, opaque red, as one
## RGBA8 pixel read as a little-endian 32-bit integer.
const DIFFERENCE_RED := -16776961
## The most instances one stress window adds, and its longest window.
const MAX_STRESS_INSTANCES := 1000
const MAX_STRESS_MS := 10000
## The monitors a stress window samples, by their Performance.Monitor names.
const STRESS_MONITORS := [
	"TIME_FPS",
	"TIME_PROCESS",
	"TIME_PHYSICS_PROCESS",
	"OBJECT_COUNT",
	"OBJECT_NODE_COUNT",
	"OBJECT_ORPHAN_NODE_COUNT",
]
## The counts a stress window reads before, during and after it.
const STRESS_COUNTS := ["OBJECT_COUNT", "OBJECT_NODE_COUNT", "OBJECT_ORPHAN_NODE_COUNT"]
## The longest text a check answers; the comparison reads all of it.
const MAX_CHECK_TEXT := 4096

## The reference image being uploaded, or the complete upload the next
## comparison reads: {"upload", "total", "sha256", "bytes", "complete"}.
var _reference: Dictionary = {}


## The text a Control shows, as runtime.ui reports it, or null when it has no
## `text` property.
static func _control_text(control: Control):
	if "text" in control:
		return str(control.get("text"))
	return null


## `runtime.image.reference`: one chunk of the reference image the next
## comparison reads. A chunk at offset 0 starts an upload and replaces any
## other; each later chunk continues it where it stands, and the last one
## completes it once the bytes match the upload's SHA-256.
func _image_reference(payload: Dictionary) -> Dictionary:
	var upload := str(payload.get("upload", ""))
	var digest := str(payload.get("sha256", ""))
	var total: int = int(payload.get("total", 0))
	var offset: int = int(payload.get("offset", -1))
	if upload == "" or digest.length() != 64:
		return fail("invalid_input", "an upload names itself and the SHA-256 of its bytes")
	if total < 1 or total > MAX_REFERENCE_BYTES:
		return fail("invalid_input", "a reference image is 1 to %d bytes" % MAX_REFERENCE_BYTES)
	var data: PackedByteArray = Marshalls.base64_to_raw(str(payload.get("data_base64", "")))
	if data.is_empty() or data.size() > MAX_CHUNK_BYTES:
		return fail("invalid_input", "a chunk carries 1 to %d bytes" % MAX_CHUNK_BYTES)
	if offset == 0:
		_reference = {"upload": upload, "total": total, "sha256": digest, "bytes": PackedByteArray(), "complete": false}
	var held: PackedByteArray = _reference.get("bytes", PackedByteArray())
	if str(_reference.get("upload", "")) != upload or bool(_reference.get("complete", true)) \
			or int(_reference.get("total", -1)) != total or str(_reference.get("sha256", "")) != digest \
			or held.size() != offset:
		return fail("invalid_input", "the chunk at %d does not continue upload %s" % [offset, upload])
	# The upload's own reference is dropped first, so the bytes grow in place.
	_reference["bytes"] = PackedByteArray()
	held.append_array(data)
	if held.size() > total:
		_reference = {}
		return fail("invalid_input", "upload %s carries more than its %d bytes" % [upload, total])
	_reference["bytes"] = held
	if held.size() == total:
		var hashing := HashingContext.new()
		hashing.start(HashingContext.HASH_SHA256)
		hashing.update(held)
		if hashing.finish().hex_encode() != digest:
			_reference = {}
			return fail("invalid_input", "upload %s does not match its SHA-256" % upload)
		_reference["complete"] = true
	return ok({"upload": upload, "received": held.size(), "total": total, "complete": bool(_reference["complete"])})


## `runtime.image.compare`: capture the root viewport after the next drawn
## frame and compare it with the uploaded reference, both as RGBA8: a pixel
## differs when one of its four channels differs by more than `tolerance`.
## Images of two sizes are not compared, and nothing is resized. The capture,
## and a difference image when pixels differ and `difference` asks for one,
## are kept as one capture whose frames the server reads back. The upload is
## used once.
func _image_compare(payload: Dictionary, message_id: String) -> Dictionary:
	var epoch := _epoch
	var upload := str(payload.get("upload", ""))
	var held: Dictionary = _reference
	_reference = {}
	if DisplayServer.get_name() == "headless":
		return fail("unavailable", "this session renders nothing: it runs headless")
	if upload == "" or str(held.get("upload", "")) != upload or not bool(held.get("complete", false)):
		return fail("invalid_input", "upload %s is not a complete reference image" % upload)
	var tolerance: int = int(payload.get("tolerance", 0))
	if tolerance < 0 or tolerance > 255:
		return fail("invalid_input", "`tolerance` is 0 to 255")
	var marking := bool(payload.get("difference", true))
	var reference_bytes: PackedByteArray = held["bytes"]
	var reference := Image.new()
	var loaded: int = reference.load_png_from_buffer(reference_bytes)
	if loaded != OK:
		return fail("invalid_input", "the reference is not a PNG image this Godot build decodes (error %d)" % loaded)
	await RenderingServer.frame_post_draw
	var image: Image = _grab(0)
	if image == null:
		return fail("unavailable", "the root viewport produced no image")
	var capture_png: PackedByteArray = image.save_png_to_buffer()
	var width := image.get_width()
	var height := image.get_height()
	var same_size := reference.get_width() == width and reference.get_height() == height
	var answer := {
		"capture_size": {"width": width, "height": height},
		"reference": {"width": reference.get_width(), "height": reference.get_height(),
			"byte_length": reference_bytes.size(), "sha256": str(held["sha256"])},
		"tolerance": tolerance,
		"same_size": same_size,
		"total_pixels": width * height,
		"differing_pixels": null,
		"ratio": null,
		"max_channel_difference": null,
		"bounding_box": null,
		"difference_frame": null,
	}
	var frames: Array = [capture_png]
	if not same_size:
		answer["matched"] = false
		answer["reason"] = "the capture is %dx%d pixels and the reference %dx%d; images of two sizes are not compared" % [
			width, height, reference.get_width(), reference.get_height()]
	else:
		var compared: Dictionary = await _differences(image, reference, tolerance, marking, message_id, epoch)
		if compared.has("error"):
			return compared["error"]
		var differing: int = compared["differing"]
		answer["differing_pixels"] = differing
		answer["ratio"] = float(differing) / float(maxi(1, width * height))
		answer["max_channel_difference"] = compared["worst"]
		answer["bounding_box"] = compared["box"]
		answer["matched"] = differing == 0
		answer["reason"] = null if differing == 0 else "%d of %d pixels differ by more than %d in a channel" % [
			differing, width * height, tolerance]
		if compared["difference"] != null:
			frames.append((compared["difference"] as Image).save_png_to_buffer())
			answer["difference_frame"] = 1
	_cancelled.erase(message_id)
	answer.merge(_keep({"frames": frames, "width": width, "height": height}))
	return ok(_observed(answer))


## How `image` differs from `reference`, both of one size: {"differing",
## "worst", "box", "difference": Image or null} or {"error": refusal}. Rows
## equal byte for byte are skipped natively; the work lets a frame pass every
## COMPARE_SLICE_MS, and a withdrawal then ends it.
func _differences(image: Image, reference: Image, tolerance: int, marking: bool, message_id: String, epoch: int) -> Dictionary:
	var width := image.get_width()
	var height := image.get_height()
	image.convert(Image.FORMAT_RGBA8)
	reference.convert(Image.FORMAT_RGBA8)
	if image.has_mipmaps():
		image.clear_mipmaps()
	if reference.has_mipmaps():
		reference.clear_mipmaps()
	var first: PackedByteArray = image.get_data()
	var second: PackedByteArray = reference.get_data()
	if first == second:
		return {"differing": 0, "worst": 0, "box": null, "difference": null}
	var pixels: PackedInt32Array = first.to_int32_array()
	var expected: PackedInt32Array = second.to_int32_array()
	var marked := PackedInt32Array()
	if marking:
		var shade := image.duplicate() as Image
		shade.adjust_bcs(0.35, 1.0, 0.0)
		marked = shade.get_data().to_int32_array()
	var differing := 0
	var worst := 0
	var left := width
	var top := height
	var right := -1
	var bottom := -1
	var slice_started := Time.get_ticks_msec()
	for y in height:
		var start := y * width
		if pixels.slice(start, start + width) != expected.slice(start, start + width):
			for x in width:
				var p: int = pixels[start + x]
				var q: int = expected[start + x]
				if p == q:
					continue
				var d: int = maxi(
					maxi(absi((p & 255) - (q & 255)), absi(((p >> 8) & 255) - ((q >> 8) & 255))),
					maxi(absi(((p >> 16) & 255) - ((q >> 16) & 255)), absi(((p >> 24) & 255) - ((q >> 24) & 255))))
				if d > worst:
					worst = d
				if d > tolerance:
					differing += 1
					left = mini(left, x)
					right = maxi(right, x)
					top = mini(top, y)
					bottom = maxi(bottom, y)
					if marking:
						marked[start + x] = DIFFERENCE_RED
		if Time.get_ticks_msec() - slice_started >= COMPARE_SLICE_MS:
			await get_tree().process_frame
			var interrupted := _interrupted(message_id, epoch)
			if interrupted != "":
				_cancelled.erase(message_id)
				return {"error": fail("cancelled", "the comparison was withdrawn (%s)" % interrupted)}
			slice_started = Time.get_ticks_msec()
	var difference: Image = null
	if marking and differing > 0:
		difference = Image.create_from_data(width, height, false, Image.FORMAT_RGBA8, marked.to_byte_array())
	var box = null
	if differing > 0:
		box = {"x": left, "y": top, "width": right - left + 1, "height": bottom - top + 1}
	return {"differing": differing, "worst": worst, "box": box, "difference": difference}


## `runtime.scenario.check`: one assertion of a test scenario, decided here
## from the live value so it follows the engine's own arithmetic: a
## `property` of a node or an allowed singleton compared with `value` by `op`
## exactly as runtime.wait compares, or the `ui_text` of a Control, the text
## runtime.ui reports, matched exactly or as a part. `holds` is null, with the
## reason, when the comparison does not apply to the value.
func _scenario_check(payload: Dictionary) -> Dictionary:
	match str(payload.get("kind", "")):
		"property":
			return _check_property(payload)
		"ui_text":
			return _check_ui_text(payload)
	return fail("invalid_input", "a check is of kind property or ui_text")


func _check_property(payload: Dictionary) -> Dictionary:
	var target: Dictionary = _target_of(payload)
	if target.has("error"):
		return target["error"]
	var property := str(payload.get("property", ""))
	var object: Object = target["object"]
	if target["singleton"]:
		if not SINGLETON_PROPERTIES[target["name"]].has(property):
			return fail("invalid_input", "only %s of %s are read here" % [
				", ".join(SINGLETON_PROPERTIES[target["name"]].keys()), target["name"]])
	elif not _property_names(object, [property], 1)["missing"].is_empty():
		return fail("not_found", "%s has no property %s" % [target["name"], property])
	var decoded: Dictionary = VariantCodec.decode(payload.get("value"))
	if not decoded.get("ok", false):
		return fail("invalid_input", "`value`: %s" % str(decoded.get("reason", "not a tagged value")))
	var live = object.get(property)
	var compared: Dictionary = Values.compare(live, str(payload.get("op", "")), decoded["value"], float(payload.get("tolerance", 0.0)))
	var answer := {"class": target["class"], "property": property, "op": str(payload.get("op", ""))}
	answer["singleton" if target["singleton"] else "node"] = target["name"]
	answer["holds"] = compared["value"] if compared["ok"] else null
	answer["reason"] = null if compared["ok"] else str(compared["reason"])
	answer.merge(_shown(live))
	return ok(_observed(answer))


func _check_ui_text(payload: Dictionary) -> Dictionary:
	var node: Node = _resolve(payload.get("node", ""))
	if node == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("node", "")))
	if not (node is Control):
		return fail("invalid_input", "%s is a %s, not a Control" % [str(node.get_path()), node.get_class()])
	var text = _control_text(node as Control)
	if text == null:
		return fail("invalid_input", "%s is a %s, which has no text property" % [str(node.get_path()), node.get_class()])
	var shown: String = text
	var wanted := str(payload.get("text", ""))
	var contains := str(payload.get("match", "exact")) == "contains"
	return ok(_observed({
		"node": str(node.get_path()),
		"class": node.get_class(),
		"visible": (node as Control).is_visible_in_tree(),
		"match": "contains" if contains else "exact",
		"holds": shown.contains(wanted) if contains else shown == wanted,
		"reason": null,
		"text": shown.substr(0, MAX_CHECK_TEXT),
		"text_length": shown.length(),
		"truncated": shown.length() > MAX_CHECK_TEXT,
	}))


## A live value as a check shows it: tagged when it has a JSON spelling, else
## why not; a value that is or holds a float that is not finite is named so.
static func _shown(live) -> Dictionary:
	var shown := {"value": null, "special": null, "type": type_string(typeof(live)), "unrepresentable": null}
	var special := _non_finite(live)
	if special != "":
		shown["special"] = special
		return shown
	var encoded: Dictionary = VariantCodec.encode(live)
	if encoded.get("ok", false):
		shown["value"] = encoded["value"]
	else:
		shown["unrepresentable"] = str(encoded.get("reason", "not representable"))
	return shown


## "nan", "inf" or "-inf" for a float that is not finite, "non_finite" for a
## value with such a component, "" otherwise.
static func _non_finite(value) -> String:
	if typeof(value) == TYPE_FLOAT:
		if is_nan(value):
			return "nan"
		if is_inf(value):
			return "inf" if value > 0.0 else "-inf"
		return ""
	for component in _reals(value):
		if is_nan(component) or is_inf(component):
			return "non_finite"
	return ""


## The real components of a vector, colour, quaternion or other real-valued
## math type, in order; [] for any other value.
static func _reals(value) -> Array:
	match typeof(value):
		TYPE_VECTOR2:
			return [value.x, value.y]
		TYPE_VECTOR3:
			return [value.x, value.y, value.z]
		TYPE_VECTOR4, TYPE_QUATERNION:
			return [value.x, value.y, value.z, value.w]
		TYPE_COLOR:
			return [value.r, value.g, value.b, value.a]
		TYPE_RECT2:
			return [value.position.x, value.position.y, value.size.x, value.size.y]
		TYPE_PLANE:
			return [value.normal.x, value.normal.y, value.normal.z, value.d]
		TYPE_AABB:
			return [value.position.x, value.position.y, value.position.z, value.size.x, value.size.y, value.size.z]
		TYPE_TRANSFORM2D:
			return [value.x.x, value.x.y, value.y.x, value.y.y, value.origin.x, value.origin.y]
		TYPE_BASIS:
			return _reals(value.x) + _reals(value.y) + _reals(value.z)
		TYPE_TRANSFORM3D:
			return _reals(value.basis) + _reals(value.origin)
		TYPE_PROJECTION:
			return _reals(value.x) + _reals(value.y) + _reals(value.z) + _reals(value.w)
	return []


## `runtime.scenario.stress`: add `count` instances of the project scene
## `scene` under `parent`, sample the stress monitors every `interval_ms` for
## `duration_ms` as runtime.sample does, then free every instance and count
## the ones still there two frames later. The object, node and orphan node
## counts are read before, during and after. A withdrawal ends the window
## early; the instances are freed all the same.
func _scenario_stress(payload: Dictionary, message_id: String) -> Dictionary:
	var scene := str(payload.get("scene", ""))
	var count: int = int(payload.get("count", 0))
	var duration: int = int(payload.get("duration_ms", 1000))
	var interval: int = int(payload.get("interval_ms", 100))
	if not scene.begins_with("res://") or count < 1 or count > MAX_STRESS_INSTANCES \
			or duration < 1 or duration > MAX_STRESS_MS or interval < 1 or interval > MAX_SAMPLE_INTERVAL_MS:
		return fail("invalid_input", "a stress window adds 1 to %d instances of a res:// scene for 1 to %d ms, sampled every 1 to %d ms" % [
			MAX_STRESS_INSTANCES, MAX_STRESS_MS, MAX_SAMPLE_INTERVAL_MS])
	var parent: Node = _resolve(payload.get("parent", "."))
	if parent == null:
		return fail("not_found", "%s names no node of the running game" % str(payload.get("parent", ".")))
	if not ResourceLoader.exists(scene):
		return fail("not_found", "%s is not a resource of the project" % scene)
	var loaded = ResourceLoader.load(scene)
	if not (loaded is PackedScene):
		return fail("invalid_input", "%s is not a scene" % scene)
	var packed := loaded as PackedScene
	var builtin := _builtin_monitors()
	var series: Array = []
	for name in STRESS_MONITORS:
		if builtin.has(name):
			series.append({"monitor": name})
	var before := _stress_counts(builtin)
	var instances: Array = []
	for index in count:
		var instance: Node = packed.instantiate()
		if instance == null:
			_free_all(instances)
			return fail("internal", "%s did not instantiate (instance %d of %d); the ones added were freed" % [scene, index + 1, count],
				"not_started" if instances.is_empty() else "committed")
		parent.add_child(instance)
		instances.append(instance)
	await get_tree().process_frame
	var sampled: Dictionary = await _sample({"series": series, "duration_ms": duration, "interval_ms": interval}, message_id)
	var present := 0
	for instance in instances:
		if is_instance_valid(instance) and (instance as Node).is_inside_tree():
			present += 1
	var during := _stress_counts(builtin)
	_free_all(instances)
	for frame in 2:
		await get_tree().process_frame
	var remaining := 0
	for instance in instances:
		if is_instance_valid(instance):
			remaining += 1
	var after := _stress_counts(builtin)
	_cancelled.erase(message_id)
	if not sampled.get("ok", false):
		return fail(str(sampled.get("error_kind", "internal")), "%s; the instances were freed" % str(sampled.get("message", "the sample failed")), "committed")
	var observed: Dictionary = sampled["payload"]
	var monitors := {}
	for entry in observed.get("series", []):
		monitors[str(entry.get("monitor", ""))] = entry.get("stats")
	var stopped_by := str(observed.get("stopped_by", "duration"))
	var taken: Array = observed.get("samples", [])
	return ok(_observed({
		"scene": scene,
		"parent": str(parent.get_path()) if is_instance_valid(parent) else null,
		"requested": count,
		"created": instances.size(),
		"present_during": present,
		"remaining": remaining,
		"freed": instances.size() - remaining,
		"outcome": stopped_by if stopped_by == "cancelled" or stopped_by == "interrupted" else "completed",
		"duration_ms": duration,
		"interval_ms": interval,
		"samples": taken.size(),
		"paused_samples": observed.get("paused_samples", 0),
		"monitors": monitors,
		"counts": {"before": before, "during": during, "after": after},
	}), "committed")


static func _stress_counts(builtin: Dictionary) -> Dictionary:
	var counts := {}
	for name in STRESS_COUNTS:
		counts[name] = _finite(Performance.get_monitor(builtin[name])) if builtin.has(name) else null
	return counts


static func _free_all(instances: Array) -> void:
	for instance in instances:
		if is_instance_valid(instance):
			(instance as Node).queue_free()
