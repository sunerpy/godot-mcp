extends SceneTree
## Off-main-thread refusal: the godot_mcp dispatcher must refuse editor work
## requested from any thread but the main one, and run it on the main thread.
## Run headless with the add-on copied into the project:
##     godot --headless --path <project> --script res://main_thread_probe.gd

const CommandQueue = preload("res://addons/godot_mcp/command_queue.gd")


class StubHandlers:
	extends RefCounted
	var calls := 0

	func supports(_method: String) -> bool:
		return true

	func is_mutation(_method: String) -> bool:
		return false

	func dispatch(_method: String, _payload: Dictionary, _context: Dictionary) -> Dictionary:
		calls += 1
		return {"ok": true, "payload": {"ran": true}}


var _handlers := StubHandlers.new()
var _queue := CommandQueue.new(_handlers)
var _thread_answer: Dictionary = {}


func _request() -> Dictionary:
	return {"message_id": "probe-1", "method": "editor.state", "operation_class": "read",
		"deadline_unix_ms": "0", "payload": {}}


func _off_main() -> void:
	_thread_answer = _queue.execute(_request())


func _init() -> void:
	var thread := Thread.new()
	thread.start(_off_main)
	thread.wait_to_finish()
	var off_main_calls := _handlers.calls
	var main_answer: Dictionary = _queue.execute(_request())
	print("MAIN_THREAD_PROBE ", JSON.stringify({
		"off_main": _thread_answer,
		"off_main_handler_calls": off_main_calls,
		"main": main_answer,
		"main_handler_calls": _handlers.calls,
	}))
	quit(0)
