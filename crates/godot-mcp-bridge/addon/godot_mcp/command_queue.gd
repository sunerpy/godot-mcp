@tool
extends RefCounted
## The main-thread dispatcher.
##
## The network side only parses bounded frames and enqueues validated request
## envelopes here. `drain()` runs from a deferred main-thread call and, for each
## queued request in order:
##
## 1. rechecks that it runs on the main thread, that the request's deadline has
##    not passed and that it was not cancelled;
## 2. hands the method and its payload to the handler set, which validates the
##    typed arguments, resolves node handles and executes the read or builds
##    one undoable mutation;
## 3. answers with the handler's typed payload and one of the three mutation
##    statuses: `committed`, `not_started` or `unresolved`.
##
## A handler may span frames (it awaits, for example to let the editor settle);
## the requests behind it wait, so requests still run strictly one at a time
## and in order.
##
## Editor work never runs anywhere but the main thread: `execute()` refuses,
## with `not_started`, a call made from any other thread. A queued request that
## has not started when the connection is lost is dropped, never run later, and
## a dropped mutation is never replayed by this side.

const MAX_QUEUED := 64

var handlers = null
var _queue: Array = []
var _draining := false


func _init(handler_set = null) -> void:
	handlers = handler_set


func size() -> int:
	return _queue.size()


## Queue one validated request envelope. Returns "" or the refusal reason.
func enqueue(envelope: Dictionary) -> String:
	if _queue.size() >= MAX_QUEUED:
		return "the editor bridge queue is full"
	_queue.append(envelope)
	return ""


## Withdraw a queued request that has not started. Returns the envelope, or {}
## when nothing with that message id is waiting.
func cancel(message_id: String) -> Dictionary:
	for index in _queue.size():
		if str(_queue[index].get("message_id", "")) == message_id:
			var withdrawn: Dictionary = _queue[index]
			_queue.remove_at(index)
			return withdrawn
	return {}


## Drop every queued request; they will never run.
func drop_all() -> Array:
	var dropped: Array = _queue.duplicate()
	_queue.clear()
	return dropped


## Execute every queued request in order, one at a time, handing each answer
## to `reply`. A drain that is already running picks up what was queued since.
func drain(reply: Callable) -> void:
	if _draining:
		return
	_draining = true
	while not _queue.is_empty():
		var envelope: Dictionary = _queue.pop_front()
		var answer: Dictionary = await run(envelope)
		reply.call(envelope, answer)
	_draining = false


static func now_unix_ms() -> int:
	return int(Time.get_unix_time_from_system() * 1000.0)


static func is_main_thread() -> bool:
	return OS.get_thread_caller_id() == OS.get_main_thread_id()


static func refusal(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


## Execute one request envelope whose handler answers at once. Returns
## {"ok": true, "payload": {...}, "mutation_status": ...} or a refusal
## {"ok": false, "error_kind", "message", "mutation_status"}.
func execute(envelope: Dictionary) -> Dictionary:
	var admitted: Dictionary = _admit(envelope)
	if admitted.has("refusal"):
		return admitted["refusal"]
	return _settled(handlers.dispatch(admitted["method"], admitted["payload"], admitted["context"]), admitted["mutation"])


## Execute one request envelope, awaiting a handler that spans frames.
func run(envelope: Dictionary) -> Dictionary:
	var admitted: Dictionary = _admit(envelope)
	if admitted.has("refusal"):
		return admitted["refusal"]
	var answer: Dictionary = await handlers.dispatch(admitted["method"], admitted["payload"], admitted["context"])
	return _settled(answer, admitted["mutation"])


## The checks every request passes before its handler runs: the main thread,
## its deadline, a typed payload, an implemented method and the right
## operation class. Returns {"refusal": ...} or what the handler needs.
func _admit(envelope: Dictionary) -> Dictionary:
	if not is_main_thread():
		return {"refusal": refusal("internal", "editor bridge work runs only on the editor main thread")}
	var deadline: int = int(str(envelope.get("deadline_unix_ms", "0")))
	if deadline > 0 and now_unix_ms() >= deadline:
		return {"refusal": refusal("timeout", "the request's deadline passed before it started")}
	if handlers == null:
		return {"refusal": refusal("unavailable", "the editor bridge has no handlers")}
	var method: String = str(envelope.get("method", ""))
	var payload = envelope.get("payload")
	if typeof(payload) != TYPE_DICTIONARY:
		return {"refusal": refusal("invalid_input", "a request payload must be an object")}
	var mutation: bool = str(envelope.get("operation_class", "")) == "mutation"
	if not handlers.supports(method):
		return {"refusal": refusal("unsupported", "the editor bridge does not implement %s" % method)}
	if handlers.is_mutation(method) != mutation:
		return {"refusal": refusal("invalid_input", "%s was sent with the wrong operation class" % method)}
	return {
		"method": method,
		"payload": payload,
		"mutation": mutation,
		"context": {
			"deadline_unix_ms": deadline,
			"idempotency_id": str(envelope.get("idempotency_id", "")),
			"message_id": str(envelope.get("message_id", "")),
		},
	}


static func _settled(answer: Dictionary, mutation: bool) -> Dictionary:
	if not answer.has("mutation_status"):
		answer["mutation_status"] = "committed" if mutation and answer.get("ok", false) else "not_started"
	return answer
