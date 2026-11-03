@tool
extends RefCounted
## The editor's log since this plugin started, for `editor.log.read`: a ring
## of the most recent entries that a `Logger` fills.
##
## `Logger` and `OS.add_logger` exist from Godot 4.5. A script that extends
## `Logger` does not parse on Godot 4.4, so the logger is compiled from
## LOGGER_SOURCE at start, and only where the class exists; on an older build
## nothing is captured and `editor.log.read` is not offered.
##
## A logger can be called from any thread that prints, so the ring is guarded
## by a mutex, and nothing here prints or pushes an error: an entry logged
## while one is recorded would come straight back.

const CAPACITY := 2000
const MAX_TEXT := 4096
const MAX_FRAMES := 16
## One answer stays far below the bridge's 4 MiB frame.
const BUDGET_BYTES := 1048576
const KINDS := ["error", "warning", "script", "shader"]
const LOGGER_SOURCE := """extends Logger

var sink = null


func _log_message(message: String, error: bool) -> void:
	if sink != null:
		sink.message(message, error)


func _log_error(function: String, file: String, line: int, code: String, rationale: String, editor_notify: bool, error_type: int, script_backtraces: Array) -> void:
	if sink != null:
		sink.error(function, file, line, code, rationale, error_type, script_backtraces)
"""

## Changes whenever the plugin starts, so a reader can tell that sequence
## numbers began again.
var epoch := ""
var _logger = null
var _mutex := Mutex.new()
var _ring: Array = []
## How many entries were ever recorded: the newest entry's sequence number.
var _total := 0


func _init() -> void:
	_ring.resize(CAPACITY)
	epoch = "%x%x" % [Time.get_ticks_usec(), randi()]


## Start capturing. False, and nothing captured, on a Godot without `Logger`.
func start() -> bool:
	if _logger != null:
		return true
	if not ClassDB.class_exists("Logger") or not OS.has_method("add_logger"):
		return false
	var script := GDScript.new()
	script.source_code = LOGGER_SOURCE
	if script.reload() != OK:
		return false
	_logger = script.new()
	_logger.sink = self
	OS.call("add_logger", _logger)
	return true


func stop() -> void:
	if _logger == null:
		return
	OS.call("remove_logger", _logger)
	_logger.sink = null
	_logger = null


func capturing() -> bool:
	return _logger != null


## The newest entry's sequence number, 0 before any: a read `after` it returns
## only what is logged from now on.
func last() -> int:
	_mutex.lock()
	var total := _total
	_mutex.unlock()
	return total


func message(text: String, error: bool) -> void:
	_record({
		"kind": "message",
		"text": text.trim_suffix("\n").left(MAX_TEXT),
		"stderr": error,
		"function": null,
		"file": null,
		"line": null,
		"code": null,
		"rationale": null,
		"backtrace": [],
	})


func error(function: String, file: String, line: int, code: String, rationale: String, error_type: int, backtraces: Array) -> void:
	var frames: Array = []
	for backtrace in backtraces:
		if backtrace == null or not backtrace.has_method("get_frame_count"):
			continue
		for index in backtrace.get_frame_count():
			if frames.size() >= MAX_FRAMES:
				break
			frames.append({
				"function": str(backtrace.get_frame_function(index)).left(MAX_TEXT),
				"file": str(backtrace.get_frame_file(index)).left(MAX_TEXT),
				"line": int(backtrace.get_frame_line(index)),
			})
	_record({
		"kind": KINDS[error_type] if error_type >= 0 and error_type < KINDS.size() else "error",
		"text": (rationale if rationale != "" else code).left(MAX_TEXT),
		"stderr": true,
		"function": function.left(MAX_TEXT),
		"file": file.left(MAX_TEXT),
		"line": line,
		"code": code.left(MAX_TEXT),
		"rationale": rationale.left(MAX_TEXT),
		"backtrace": frames,
	})


func _record(entry: Dictionary) -> void:
	entry["unix_ms"] = int(Time.get_unix_time_from_system() * 1000.0)
	_mutex.lock()
	_total += 1
	entry["sequence"] = _total
	_ring[(_total - 1) % CAPACITY] = entry
	_mutex.unlock()


## `editor.log.read`: the entries after `after`, oldest first, at most
## `limit`, only `kinds` when given. A read with another plugin start's
## `epoch` starts from the oldest entry kept and says `reset`.
func read(payload: Dictionary) -> Dictionary:
	var after := maxi(int(payload.get("after", 0)), 0)
	var limit := clampi(int(payload.get("limit", 100)), 1, 500)
	var kinds = payload.get("kinds")
	var asked_epoch = payload.get("epoch")
	var reset: bool = asked_epoch != null and str(asked_epoch) != epoch
	_mutex.lock()
	var total := _total
	var oldest := maxi(total - CAPACITY + 1, 1)
	var start := oldest if reset else maxi(after + 1, oldest)
	var entries: Array = []
	var next := start - 1
	var spent := 0
	var truncated := false
	for sequence in range(start, total + 1):
		var entry: Dictionary = _ring[(sequence - 1) % CAPACITY]
		if kinds != null and not kinds.has(entry["kind"]):
			next = sequence
			continue
		var size := JSON.stringify(entry).length()
		if entries.size() >= limit or spent + size > BUDGET_BYTES:
			truncated = true
			break
		entries.append(entry.duplicate(true))
		spent += size
		next = sequence
	_mutex.unlock()
	return {
		"entries": entries,
		"next": maxi(next, 0),
		"oldest": oldest if total > 0 else null,
		"dropped": maxi(total - CAPACITY, 0),
		"skipped": 0 if reset else maxi(oldest - (after + 1), 0),
		"capacity": CAPACITY,
		"epoch": epoch,
		"reset": reset,
		"truncated": truncated,
	}
