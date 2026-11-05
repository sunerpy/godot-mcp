@tool
extends RefCounted
## What the godot_mcp panel shows, kept apart from how it is drawn.
##
## The bridge client reports the connection, each request it answered and the
## server's identity; the server reports what it serves through
## `bridge.server_status`. This keeps a bounded record of all of it and
## answers the dispatcher's one question, whether the user has paused agent
## edits.
##
## Nothing here holds a request's argument values or a file's contents. A
## request is remembered by its method, its operation class, a short target
## naming the scene, node or script path it was about, and its answer's error
## kind and mutation status. The rendezvous secret never reaches this object.

## The panel's parts that changed: "status", "activity", "tools", "controls".
signal changed(part: String)

## Rows the activity list keeps; older rows are dropped, and counted.
const MAX_ACTIVITY := 500
## Failed answers the diagnostics keep.
const MAX_ERRORS := 20
## Longest target shown, in characters; a longer one keeps both its ends.
const MAX_TARGET_CHARS := 160
## Most tools one server status may list.
const MAX_TOOLS := 512
## Most needs one server status may describe.
const MAX_NEEDS := 16
const MAX_ERROR_MESSAGE := 300
const MAX_REASON := 1024
const PAUSED_MESSAGE := "agent edits are paused in this editor: the user turned on \"Pause agent edits\" in the Godot MCP panel. Nothing was changed. Reads still work; once the user turns it off, retry with a new idempotencyId"
## What a tool needs, in the order the panel lists the groups.
const NEEDS_ORDER := ["project_files", "headless_engine", "live_editor", "game_session", "editor_session"]
## The target of a method whose target is a fixed part of the editor.
const FIXED_TARGETS := {
	"editor.addon.reload": "godot_mcp add-on",
	"editor.camera.read": "editor cameras",
	"editor.camera.set": "editor camera",
	"editor.capture.viewport": "editor viewport",
	"editor.dialog.respond": "editor dialog",
	"editor.dialogs.read": "editor dialogs",
	"editor.filesystem.scan": "project files",
	"editor.log.read": "editor log",
	"editor.main_screen.set": "main screen",
	"editor.performance.read": "editor monitors",
	"editor.preferences.read": "editor settings",
	"editor.preferences.set": "editor settings",
	"editor.restart": "editor process",
	"editor.settings.apply": "project.godot",
	"editor.shortcuts.read": "editor shortcuts",
	"editor.shortcuts.set": "editor shortcuts",
}
const PLAY_TARGETS := {"main": "main scene", "current": "edited scene"}
const ACTIVITY_COLUMNS := ["time", "method", "class", "target", "result", "mutation", "ms", "delivered"]

# --- the add-on ------------------------------------------------------------------

var addon_version := ""
var godot_version := ""
var addon_reads: Array = []
var addon_mutations: Array = []

# --- the connection --------------------------------------------------------------

## The bridge client's status line: "connected", "connecting", "disconnected: ...".
var connection := ""
var connected := false
var last_disconnect := ""
var server_version := ""
var session_id := ""
var project_generation := ""
var connected_since_unix_ms := 0
## The methods the server's capabilities listed for this session.
var server_methods: Array = []

# --- requests --------------------------------------------------------------------

var activity: Array = []
var dropped := 0
var served := 0
var failed := 0
var refused := 0
var errors: Array = []
var last_request_unix_ms := 0

# --- what the server serves ------------------------------------------------------

var server_status: Dictionary = {}
var server_status_refusal := ""
## Engine milliseconds when this session's status first arrived, or -1.
var server_status_ticks_msec := -1
## Engine milliseconds when any status first arrived since the add-on started.
var first_status_ticks_msec := -1

var _paused := false


func set_addon(version: String, reads: Array, mutations: Array) -> void:
	addon_version = version
	godot_version = str(Engine.get_version_info().get("string", ""))
	addon_reads = reads.duplicate()
	addon_mutations = mutations.duplicate()
	changed.emit("status")


# --- the pause switch the dispatcher consults ------------------------------------


func set_paused(paused: bool) -> void:
	if paused == _paused:
		return
	_paused = paused
	changed.emit("controls")


func mutations_paused() -> bool:
	return _paused


func paused_message() -> String:
	return PAUSED_MESSAGE


# --- the bridge client's signals -------------------------------------------------


func on_status(text: String) -> void:
	connection = text
	changed.emit("status")


## A session opened. `summary` names the server's version, the session id,
## the project generation and the methods the server listed.
func on_session_opened(summary: Dictionary) -> void:
	connected = true
	server_version = bounded(str(summary.get("server_version", "")), 64)
	session_id = bounded(str(summary.get("session_id", "")), 64)
	project_generation = bounded(str(summary.get("project_generation", "")), 32)
	server_methods = []
	var methods = summary.get("methods", [])
	if typeof(methods) == TYPE_ARRAY:
		for method in methods:
			if typeof(method) == TYPE_STRING and server_methods.size() < 256:
				server_methods.append(bounded(method, 96))
	connected_since_unix_ms = now_unix_ms()
	server_status = {}
	server_status_refusal = ""
	server_status_ticks_msec = -1
	changed.emit("status")
	changed.emit("tools")


func on_disconnected(reason: String) -> void:
	var was_connected := connected
	connected = false
	last_disconnect = bounded(reason, 300)
	server_status = {}
	server_status_refusal = ""
	server_status_ticks_msec = -1
	changed.emit("status")
	if was_connected:
		changed.emit("tools")


func on_request_answered(request: Dictionary, answer: Dictionary, duration_ms: int, delivered: bool) -> void:
	record(request, answer, duration_ms, delivered)


# --- activity --------------------------------------------------------------------


## Remember one answered request: its method, class, target and answer, never
## its arguments. The session's own bookkeeping, the `bridge.*` methods, is
## listed but not counted.
func record(request: Dictionary, answer: Dictionary, duration_ms: int, delivered: bool) -> void:
	var method := bounded(str(request.get("method", "")), 96)
	var operation_class := str(request.get("operation_class", ""))
	if not ["read", "mutation", "control"].has(operation_class):
		operation_class = ""
	var mutation := operation_class == "mutation"
	var ok := _is_true(answer.get("ok", false))
	var kind := "ok" if ok else bounded(str(answer.get("error_kind", "internal")), 32)
	var status := bounded(str(answer.get("mutation_status", "not_started")), 16) if mutation else ""
	var before_handler := not ok and _is_true(answer.get("refused", false))
	var now := now_unix_ms()
	var row := {
		"time_unix_ms": now,
		"method": method,
		"class": operation_class,
		"target": target_of(method, request.get("payload")),
		"result": kind,
		"mutation_status": status,
		"duration_ms": duration_ms,
		"delivered": delivered,
		"refused": before_handler,
		"attention": mutation and (status == "unresolved" or (not delivered and status != "not_started")),
	}
	activity.append(row)
	while activity.size() > MAX_ACTIVITY:
		activity.pop_front()
		dropped += 1
	if not method.begins_with("bridge."):
		if ok:
			served += 1
		elif before_handler:
			refused += 1
		else:
			failed += 1
	if not ok:
		errors.append({
			"time_unix_ms": now,
			"method": method,
			"target": row["target"],
			"kind": kind,
			"mutation_status": status,
			"message": bounded(str(answer.get("message", "")), MAX_ERROR_MESSAGE),
		})
		while errors.size() > MAX_ERRORS:
			errors.pop_front()
	last_request_unix_ms = now
	changed.emit("activity")
	changed.emit("status")


func clear_activity() -> void:
	activity.clear()
	dropped = 0
	changed.emit("activity")


## A short, display-safe name for what a request was about: the scene, node or
## script path it names (a node as `scene::node`), or the fixed part of the
## editor its method changes. Never an argument value, and only `res://` and
## `uid://` paths.
static func target_of(method: String, payload) -> String:
	if FIXED_TARGETS.has(method):
		return FIXED_TARGETS[method]
	if method.begins_with("bridge.") or typeof(payload) != TYPE_DICTIONARY:
		return ""
	var scene := ""
	var edited := false
	var scenes = payload.get("scenes")
	if method == "editor.play.start":
		var choice = payload.get("scene")
		if typeof(choice) == TYPE_STRING:
			if PLAY_TARGETS.has(choice):
				scene = PLAY_TARGETS[choice]
			elif _is_project_path(choice):
				scene = choice
	elif typeof(scenes) == TYPE_ARRAY and not scenes.is_empty():
		var first = scenes[0]
		var path = first.get("path") if typeof(first) == TYPE_DICTIONARY else first
		if scenes.size() == 1 and typeof(path) == TYPE_STRING and _is_project_path(path):
			scene = path
		else:
			scene = "%d scenes" % scenes.size()
	else:
		for key in ["scene", "path"]:
			var path = payload.get(key)
			if typeof(path) != TYPE_STRING:
				continue
			if path == "" and key == "scene":
				edited = true
			elif _is_project_path(path):
				scene = path
			break
	var node = payload.get("node")
	var nodes = payload.get("nodes")
	var text := scene if scene != "" else ("edited scene" if edited else "")
	if typeof(node) == TYPE_STRING and node != "":
		if scene != "":
			text = "%s::%s" % [scene, node]
		elif edited:
			text = "%s (edited scene)" % node
		else:
			text = node
	elif typeof(nodes) == TYPE_ARRAY:
		var count := "%d nodes" % nodes.size()
		if scene != "":
			text = "%s in %s" % [count, scene]
		elif edited:
			text = "%s (edited scene)" % count
		else:
			text = count
	return bounded(text, MAX_TARGET_CHARS)


static func _is_project_path(path: String) -> bool:
	return path.begins_with("res://") or path.begins_with("uid://")


## `text` without control or direction-override characters, at most `limit`
## characters long; a longer text keeps both its ends. Only the kept
## characters are looked at, so a huge text costs no more than a short one.
static func bounded(text: String, limit: int) -> String:
	if text.length() <= limit:
		return _printable(text)
	var tail := int((limit - 1) / 2.0)
	var head := limit - 1 - tail
	return _printable(text.substr(0, head)) + "…" + _printable(text.substr(text.length() - tail))


static func _printable(text: String) -> String:
	var clean := ""
	for index in text.length():
		var code := text.unicode_at(index)
		if code < 32 or code == 127 or (code >= 0x202A and code <= 0x202E) or (code >= 0x2066 and code <= 0x2069):
			clean += "?"
		else:
			clean += text[index]
	return clean


## Whether `value` is the boolean true; any other type is false.
static func _is_true(value) -> bool:
	return typeof(value) == TYPE_BOOL and value


static func now_unix_ms() -> int:
	return int(Time.get_unix_time_from_system() * 1000.0)


## `unix_ms` as a local wall-clock time, HH:MM:SS.
static func clock(unix_ms: int) -> String:
	var bias := int(Time.get_time_zone_from_system().get("bias", 0))
	return Time.get_time_string_from_unix_time(int(unix_ms / 1000.0) + bias * 60)


## `unix_ms` as a local date and time, YYYY-MM-DD HH:MM:SS.
static func moment(unix_ms: int) -> String:
	if unix_ms <= 0:
		return "never"
	var bias := int(Time.get_time_zone_from_system().get("bias", 0))
	return Time.get_datetime_string_from_unix_time(int(unix_ms / 1000.0) + bias * 60, true)


# --- what the server serves ------------------------------------------------------


## Take the server's `bridge.server_status` payload. Returns "" when it was
## taken, or why it was not; a refused status leaves the last one in place.
func accept_server_status(status) -> String:
	var refusal := _check_status(status)
	if refusal != "":
		server_status_refusal = refusal
		changed.emit("tools")
		return refusal
	server_status = _normalised_status(status)
	server_status_refusal = ""
	var now := Time.get_ticks_msec()
	if server_status_ticks_msec < 0:
		server_status_ticks_msec = now
	if first_status_ticks_msec < 0:
		first_status_ticks_msec = now
	changed.emit("tools")
	return ""


static func _check_status(status) -> String:
	if typeof(status) != TYPE_DICTIONARY:
		return "the server status is not an object"
	for key in ["server_version", "profile"]:
		if typeof(status.get(key)) != TYPE_STRING:
			return "the server status names no %s" % key
	var tools = status.get("tools")
	if typeof(tools) != TYPE_ARRAY:
		return "the server status lists no tools"
	if tools.size() > MAX_TOOLS:
		return "the server status lists %d tools; at most %d are shown" % [tools.size(), MAX_TOOLS]
	for tool in tools:
		if typeof(tool) != TYPE_DICTIONARY or typeof(tool.get("name")) != TYPE_STRING or tool["name"] == "":
			return "a tool in the server status has no name"
		if typeof(tool.get("needs", "")) != TYPE_STRING:
			return "the tool %s names what it needs as something other than text" % bounded(tool["name"], 96)
	var needs = status.get("needs", [])
	if typeof(needs) != TYPE_ARRAY:
		return "the server status lists its needs as something other than a list"
	if needs.size() > MAX_NEEDS:
		return "the server status describes %d needs; at most %d are shown" % [needs.size(), MAX_NEEDS]
	for need in needs:
		if typeof(need) != TYPE_DICTIONARY or typeof(need.get("needs")) != TYPE_STRING:
			return "a need in the server status is not named"
	var blocked = status.get("mutations_blocked")
	if typeof(blocked) != TYPE_NIL and typeof(blocked) != TYPE_STRING:
		return "the server status says why mutations are blocked as something other than text"
	return ""


static func _text_or_empty(value, limit: int) -> String:
	return bounded(value, limit) if typeof(value) == TYPE_STRING else ""


static func _normalised_status(status: Dictionary) -> Dictionary:
	var tools: Array = []
	for tool in status["tools"]:
		tools.append({
			"name": bounded(tool["name"], 96),
			"title": _text_or_empty(tool.get("title"), 160),
			"needs": _text_or_empty(tool.get("needs"), 32),
			"read_only": _is_true(tool.get("read_only")),
		})
	var needs: Array = []
	for need in status.get("needs", []):
		needs.append({
			"needs": bounded(need["needs"], 32),
			"source": _text_or_empty(need.get("component", need.get("plane", "")), 32),
			"status": _text_or_empty(need.get("status"), 32),
			"reason": _text_or_empty(need.get("reason"), MAX_REASON),
		})
	var total = status.get("tools_total", tools.size())
	return {
		"server_version": bounded(status["server_version"], 64),
		"profile": bounded(status["profile"], 32),
		"transport": _text_or_empty(status.get("transport"), 16),
		"mutations_blocked": _text_or_empty(status.get("mutations_blocked"), MAX_REASON),
		"tools_total": int(total) if typeof(total) in [TYPE_INT, TYPE_FLOAT] else tools.size(),
		"tools_truncated": _is_true(status.get("tools_truncated")),
		"needs": needs,
		"tools": tools,
	}


## What the Tools tab can say: "not_connected", "not_described" (the server
## does not send its status), "waiting" or "described".
func tools_state() -> String:
	if not connected:
		return "not_connected"
	if not server_status.is_empty():
		return "described"
	if server_methods.has("bridge.server_status"):
		return "waiting"
	return "not_described"


## The server's tools grouped by what they need, in NEEDS_ORDER, each group
## with that need's availability: [{needs, status, reason, tools}].
func tool_groups() -> Array:
	if server_status.is_empty():
		return []
	var availability := {}
	for need in server_status["needs"]:
		availability[need["needs"]] = need
	var by_need := {}
	for tool in server_status["tools"]:
		var need: String = tool["needs"] if tool["needs"] != "" else "unknown"
		if not by_need.has(need):
			by_need[need] = []
		by_need[need].append(tool)
	var order: Array = NEEDS_ORDER.duplicate()
	var others: Array = []
	for need in by_need.keys():
		if not order.has(need):
			others.append(need)
	others.sort()
	order.append_array(others)
	var groups: Array = []
	for need in order:
		if not by_need.has(need):
			continue
		var described: Dictionary = availability.get(need, {})
		groups.append({
			"needs": need,
			"status": str(described.get("status", "unknown")) if not described.is_empty() else "unknown",
			"reason": str(described.get("reason", "")),
			"source": str(described.get("source", "")),
			"tools": by_need[need],
		})
	return groups


# --- copies ----------------------------------------------------------------------


## The activity list as a plain-text table: a header, then one tab-separated
## row per request, oldest first.
func activity_text() -> String:
	var lines: Array = ["\t".join(ACTIVITY_COLUMNS)]
	for row in activity:
		lines.append("\t".join([
			clock(int(row["time_unix_ms"])),
			row["method"],
			row["class"],
			row["target"],
			row["result"],
			row["mutation_status"],
			str(row["duration_ms"]) if int(row["duration_ms"]) >= 0 else "",
			"yes" if row["delivered"] else "no",
		]))
	return "\n".join(lines) + "\n"


## What to paste into a bug report: versions, connection, session, counts and
## the last errors. No secret, request value or file content is part of it.
func diagnostics_text() -> String:
	var lines: Array = [
		"Godot MCP diagnostics",
		"add-on: %s" % (addon_version if addon_version != "" else "unknown"),
		"godot: %s" % (godot_version if godot_version != "" else str(Engine.get_version_info().get("string", ""))),
		"connection: %s" % (connection if connection != "" else "starting"),
		"connected: %s" % ("yes, since %s" % moment(connected_since_unix_ms) if connected else "no"),
		"last disconnect: %s" % (last_disconnect if last_disconnect != "" else "none"),
		"server: %s" % (server_version if server_version != "" else "unknown"),
		"session: %s" % (session_id if session_id != "" else "none"),
		"project generation: %s" % (project_generation if project_generation != "" else "unknown"),
		"agent edits: %s" % ("paused by the user" if _paused else "allowed"),
		"requests: %d served, %d failed, %d refused; %d listed, %d dropped from the list" % [served, failed, refused, activity.size(), dropped],
		"last request: %s" % moment(last_request_unix_ms),
	]
	match tools_state():
		"described":
			lines.append("tools: %d listed by the server (profile %s, transport %s)" % [
				server_status["tools"].size(), server_status["profile"],
				server_status["transport"] if server_status["transport"] != "" else "unknown"])
			if server_status["mutations_blocked"] != "":
				lines.append("server mutations blocked: %s" % server_status["mutations_blocked"])
		"waiting":
			lines.append("tools: the server has not described them yet")
		"not_described":
			lines.append("tools: the server did not describe its tools")
		_:
			lines.append("tools: not connected")
	if server_status_refusal != "":
		lines.append("server status refused: %s" % server_status_refusal)
	lines.append("add-on methods: %d reads, %d mutations" % [addon_reads.size(), addon_mutations.size()])
	lines.append("last errors: %s" % ("none" if errors.is_empty() else str(errors.size())))
	for error in errors:
		lines.append("  %s %s %s %s%s: %s" % [
			clock(int(error["time_unix_ms"])), error["method"], error["kind"],
			error["mutation_status"] if error["mutation_status"] != "" else "-",
			(" (%s)" % error["target"]) if error["target"] != "" else "",
			error["message"],
		])
	return "\n".join(lines) + "\n"
