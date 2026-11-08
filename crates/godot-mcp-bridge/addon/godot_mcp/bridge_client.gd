@tool
extends Node
## The peer end of the godot-mcp bridge: the editor plugin's, and a managed
## game's runtime probe's.
##
## It reads the server's private rendezvous file, dials the loopback endpoint
## it names, proves it read that file by presenting the per-launch secret and
## the project identity in `hello`, and then serves the server's requests
## through the main-thread dispatcher. Everything runs on the main thread: the
## socket is polled from `_process`, and the requests a poll received are
## executed right after it, still from `_process`. They never run inside the
## poll's receive loop, nor while deferred calls are flushed, where a windowed
## editor refuses to show the progress dialog it saves scenes with. The owner
## sets `handlers` (the method set it serves),
## `client_version` and, for a peer other than the editor plugin,
## `rendezvous_env` and `rendezvous_path` before adding the client to the tree.
##
## A connection that drops, is refused, or goes silent is closed and dialled
## again from a fresh read of the rendezvous file. Requests still queued when a
## connection is lost are dropped and never executed later, so the server's
## record of an unanswered mutation stays the only one; nothing is replayed.
##
## A server stops waiting for an answer at the request's deadline, and a
## session can end while a request runs. With `report_late_answers` set, the
## client keeps the answer to every mutation it could not hand to a waiting
## server, and that a dropped mutation never started, and reports each in a
## `mutation.late_answer` event: at once to a session already described, and
## to every session described afterwards. The server binds a report to the
## request it names or ignores it, so a report changes nothing once its
## transaction is settled.

const Codec = preload("bridge_codec.gd")
const CommandQueue = preload("command_queue.gd")

const PROTOCOL_VERSION := "1.0.0"
const SUBPROTOCOL := "godot-mcp-bridge.v1"
const SCHEMA_VERSION := "1"
const MAX_FRAME_BYTES := 4194304
const RETRY_SECONDS := 1.0
const REFUSED_RETRY_SECONDS := 5.0
const HANDSHAKE_SECONDS := 10.0
const SILENCE_SECONDS := 20.0
## Where a received request keeps when it arrived, in engine microseconds, the
## session it arrived in, and that session's server identity. Never sent: only
## the dispatcher and the answer read the request back.
const RECEIVED_KEY := "_godot_mcp_received"
## The event that reports a mutation answer no server was waiting for.
const LATE_ANSWER_EVENT := "mutation.late_answer"
## How many such answers are kept; the oldest is dropped first.
const MAX_LATE_ANSWERS := 16
## The longest message, action and digest a late answer keeps.
const MAX_LATE_MESSAGE_CHARS := 2048
const MAX_LATE_FIELD_CHARS := 256

signal status_changed(text: String)
## The connection ended: closed, refused, lost or silent. Nothing queued runs.
signal disconnected(reason: String)
## A session opened. `summary` holds the server's version, the session id,
## the project generation and the methods the server's capabilities listed.
signal session_opened(summary: Dictionary)
## A request was answered: the request envelope, the answer the dispatcher
## gave, milliseconds from its arrival to its answer (-1 when unknown), and
## whether the answer was handed to the socket.
signal request_answered(request: Dictionary, answer: Dictionary, duration_ms: int, delivered: bool)

var handlers = null
var queue = null
## What may hold mutations back before they start; handed to the dispatcher
## when the client enters the tree. See command_queue.gd.
var mutation_gate = null
## The version this peer reports in every envelope's identity.
var client_version := ""
## Where the rendezvous file is: the path in this environment variable, or,
## when it is unset, `rendezvous_path`.
var rendezvous_env := "GODOT_MCP_BRIDGE_RENDEZVOUS"
var rendezvous_path := "res://.godot-mcp/editor-bridge.rendezvous"
## Whether mutation answers no server was waiting for are kept and reported to
## the sessions described afterwards. The editor plugin sets it.
var report_late_answers := false

static var _raw_control: RegEx = null
static var _vertical_tab: RegEx = null

var _peer: WebSocketPeer = null
var _phase := "idle"
var _phase_since := 0.0
var _retry_at := 0.0
var _last_heard := 0.0
var _rendezvous: Dictionary = {}
var _session_id := ""
var _described := false
var _drain_pending := false
var _status := ""
## Counts the sessions opened, so an answer finished after its session ended
## is never sent to a later one, even one whose request has the same id.
var _session_number := 0
## The late answers kept for the next sessions, oldest first.
var _late_answers: Array = []
## Counts the late-answer events sent, so each has its own message id.
var _late_reports := 0


func _ready() -> void:
	queue = CommandQueue.new(handlers)
	queue.mutation_gate = mutation_gate
	set_process(true)


func _exit_tree() -> void:
	close("the bridge peer is stopping")


func status() -> String:
	return _status


## Whether the server is being served: connected, and the server has had the
## session described, which is when it starts sending work.
func is_connected_to_server() -> bool:
	return _phase == "ready" and _described


## How many bytes sent to the server have not yet left for the socket; 0 when
## there is no connection.
func outbound_pending() -> int:
	if _peer == null or _peer.get_ready_state() != WebSocketPeer.STATE_OPEN:
		return 0
	return _peer.get_current_outbound_buffered_amount()


static func _now() -> float:
	return Time.get_ticks_msec() / 1000.0


func _set_status(text: String) -> void:
	if text != _status:
		_status = text
		print_verbose("godot-mcp bridge: ", text)
		status_changed.emit(text)


func _process(_delta: float) -> void:
	var now := _now()
	if _phase == "idle":
		if now >= _retry_at:
			_dial()
		return
	_poll(now)
	if _drain_pending:
		_drain()


# --- rendezvous and dialling -------------------------------------------------


static func _is_hex(text: String, length: int) -> bool:
	if text.length() != length:
		return false
	for index in text.length():
		if not "0123456789abcdef".contains(text[index]):
			return false
	return true


func _read_rendezvous() -> Dictionary:
	var path := OS.get_environment(rendezvous_env)
	if path == "" and rendezvous_path != "":
		path = ProjectSettings.globalize_path(rendezvous_path)
	if path == "":
		return {}
	if not FileAccess.file_exists(path):
		return {}
	var document := Codec.decode(FileAccess.get_file_as_string(path))
	if document.is_empty() or str(document.get("schema_version", "")) != "1":
		return {}
	var endpoint := str(document.get("endpoint", ""))
	if not endpoint.begins_with("127.0.0.1:") and not endpoint.begins_with("[::1]:"):
		return {}
	if not _is_hex(str(document.get("secret_hex", "")), 64):
		return {}
	var project = document.get("project")
	if typeof(project) != TYPE_DICTIONARY:
		return {}
	for key in ["object_identity", "root_object", "file_object", "generation"]:
		if not project.has(key):
			return {}
	return document


func _dial() -> void:
	var rendezvous := _read_rendezvous()
	if rendezvous.is_empty():
		_set_status("waiting for the godot-mcp server")
		_retry_at = _now() + RETRY_SECONDS
		return
	_rendezvous = rendezvous
	_peer = WebSocketPeer.new()
	_peer.inbound_buffer_size = MAX_FRAME_BYTES
	_peer.outbound_buffer_size = MAX_FRAME_BYTES
	_peer.supported_protocols = PackedStringArray([SUBPROTOCOL])
	var error := _peer.connect_to_url("ws://%s" % str(rendezvous["endpoint"]))
	if error != OK:
		_lost("could not dial the server: %s" % error_string(error))
		return
	_phase = "connecting"
	_phase_since = _now()
	_set_status("connecting")


func _poll(now: float) -> void:
	_peer.poll()
	match _peer.get_ready_state():
		WebSocketPeer.STATE_OPEN:
			if _phase == "connecting":
				_send_hello()
				_phase = "handshaking"
				_phase_since = now
			while _peer != null and _peer.get_available_packet_count() > 0:
				var packet := _peer.get_packet()
				if not _peer.was_string_packet():
					close("binary frames are not part of the protocol")
					return
				_last_heard = now
				_on_text(packet.get_string_from_utf8())
			if _peer == null:
				return
			if _phase == "handshaking" and now - _phase_since > HANDSHAKE_SECONDS:
				_lost("the handshake timed out")
			elif _phase == "ready" and now - _last_heard > SILENCE_SECONDS:
				_lost("the server went silent")
		WebSocketPeer.STATE_CLOSED:
			_lost("the connection closed (%d)" % _peer.get_close_code())
		WebSocketPeer.STATE_CONNECTING:
			if now - _phase_since > HANDSHAKE_SECONDS:
				_lost("dialling timed out")


## Close the connection, if any, and stop serving until the next dial.
func close(reason: String) -> void:
	if _peer != null:
		if _peer.get_ready_state() == WebSocketPeer.STATE_OPEN:
			_send(_event_envelope("plugin.exiting", {"reason": reason}))
			_peer.close(1000, reason.substr(0, 120))
			_peer.poll()
		_peer = null
	if queue != null:
		_keep_dropped(queue.drop_all())
	var was_serving := _phase != "idle"
	_phase = "idle"
	if was_serving:
		disconnected.emit(reason)


## Drop the session, if any, and dial again at once from a fresh read of the
## rendezvous file.
func reconnect_now() -> void:
	close("the user asked the editor to reconnect")
	_retry_at = 0.0
	_set_status("reconnecting")


func _lost(reason: String, retry: float = RETRY_SECONDS) -> void:
	if _peer != null:
		_peer.close(1000, "closing")
		_peer = null
	# Nothing queued may run after the server lost sight of it; a later
	# session hears that the mutations among it never started.
	_keep_dropped(queue.drop_all())
	_phase = "idle"
	_retry_at = _now() + retry
	_set_status("disconnected: %s" % reason)
	disconnected.emit(reason)


# --- envelopes -----------------------------------------------------------------


func _blank(kind: String) -> Dictionary:
	var version := str(Engine.get_version_info().get("string", ""))
	return {
		"auth": null,
		"correlation_id": null,
		"deadline_unix_ms": null,
		"error": null,
		"event_name": null,
		"identity": {
			"godot_version": version.substr(0, 64),
			"plugin_version": client_version,
			"server_version": null,
			"session_id": null,
		},
		"idempotency_id": null,
		"kind": kind,
		"message_id": "",
		"method": null,
		"mutation_status": null,
		"operation_class": null,
		"origin": "plugin",
		"payload": null,
		"progress": null,
		"project": null,
		"protocol_version": PROTOCOL_VERSION,
		"reason": null,
		"schema_version": SCHEMA_VERSION,
		"sent_unix_ms": null,
	}


static func _encode(envelope: Dictionary) -> String:
	# Sorted keys, compact separators and exact floats. Every integer the plugin
	# sends is small enough for a GDScript int, so no u64 rewriting is needed.
	return strict_json(JSON.stringify(envelope, "", true, true))


## `text`, written by JSON.stringify, as JSON the server accepts.
## JSON.stringify leaves the C0 control characters other than \b \f \n \r \t
## unescaped and writes U+000B as `\v`; JSON allows neither, and the server
## refuses such a frame together with its whole session. Compact stringify
## output has no whitespace outside strings, so a raw control character is
## always inside a string, where \u00XX is its escape. A `\v` escape is one
## preceded by an even run of backslashes; an odd run is an escaped backslash.
static func strict_json(text: String) -> String:
	if _raw_control == null:
		_raw_control = RegEx.create_from_string("[\\x01-\\x1f]")
		_vertical_tab = RegEx.create_from_string("(?<!\\\\)((?:\\\\\\\\)*)\\\\v")
	if _raw_control.search(text) != null:
		for code in range(1, 32):
			text = text.replace(char(code), "\\u%04x" % code)
	if text.contains("\\v"):
		text = _vertical_tab.sub(text, "$1\\u000b", true)
	return text


func _send(envelope: Dictionary) -> bool:
	if _peer == null or _peer.get_ready_state() != WebSocketPeer.STATE_OPEN:
		return false
	var text := _encode(envelope)
	if text.to_utf8_buffer().size() > MAX_FRAME_BYTES:
		return false
	return _peer.send_text(text) == OK


func _send_hello() -> void:
	var nonce := Crypto.new().generate_random_bytes(16).hex_encode()
	var project: Dictionary = _rendezvous["project"]
	var hello := _blank("hello")
	hello["auth"] = {"nonce": nonce, "secret_hex": str(_rendezvous["secret_hex"])}
	hello["message_id"] = "hello-%s" % nonce
	hello["operation_class"] = "control"
	hello["project"] = {
		"file_object": str(project["file_object"]),
		"generation": int(str(project["generation"])),
		"object_identity": str(project["object_identity"]),
		"root_object": str(project["root_object"]),
	}
	_send(hello)


func _event_envelope(name: String, payload: Dictionary) -> Dictionary:
	var event := _blank("event")
	event["event_name"] = name
	event["message_id"] = "event-%s-%d" % [name, Time.get_ticks_usec()]
	event["payload"] = payload
	return event


func _same_project(claim) -> bool:
	if typeof(claim) != TYPE_DICTIONARY:
		return false
	var ours: Dictionary = _rendezvous["project"]
	for key in ["object_identity", "root_object", "file_object"]:
		if str(claim.get(key, "")) != str(ours.get(key, "")):
			return false
	return str(claim.get("generation", "")) == str(ours.get("generation", ""))


func _on_text(text: String) -> void:
	var message := Codec.decode(text)
	if message.is_empty() or str(message.get("origin", "")) != "server":
		_lost("the server sent a message outside the protocol")
		return
	var kind := str(message.get("kind", ""))
	match kind:
		"capabilities":
			if _phase != "handshaking" or not _same_project(message.get("project")):
				_lost("the server answered for another project")
				return
			_session_id = str(message.get("identity", {}).get("session_id", ""))
			_described = false
			_phase = "ready"
			_session_number += 1
			session_opened.emit(_session_summary(message))
			_set_status("connected")
		"error":
			if _phase == "handshaking":
				var body = message.get("error", {})
				_lost("the server refused the handshake (%s)" % str(body.get("kind", "")), REFUSED_RETRY_SECONDS)
		"request":
			if _phase != "ready":
				return
			message[RECEIVED_KEY] = [Time.get_ticks_usec(), _session_number, _session_id]
			_accept_request(message)
		"cancel":
			var correlation := str(message.get("correlation_id", ""))
			var withdrawn: Dictionary = queue.cancel(correlation)
			if not withdrawn.is_empty():
				_reply(withdrawn, CommandQueue.refused("cancelled", "the request was withdrawn before it started"))
			elif handlers != null and handlers.has_method("cancel_running"):
				# A request that already started and spans frames stops at its
				# next step; it still answers, saying what it did.
				handlers.cancel_running(correlation)
		"heartbeat":
			var reply := _blank("heartbeat")
			reply["correlation_id"] = str(message.get("message_id", ""))
			reply["message_id"] = "heartbeat-%s" % str(message.get("message_id", ""))
			reply["operation_class"] = "control"
			_send(reply)
		"shutdown":
			_lost("the server is stopping")


func _session_summary(message: Dictionary) -> Dictionary:
	var identity = message.get("identity")
	var version = identity.get("server_version") if typeof(identity) == TYPE_DICTIONARY else null
	var project = message.get("project")
	var payload = message.get("payload")
	var methods = payload.get("methods") if typeof(payload) == TYPE_DICTIONARY else null
	return {
		"server_version": version if typeof(version) == TYPE_STRING else "",
		"session_id": _session_id,
		"project_generation": str(project.get("generation", "")) if typeof(project) == TYPE_DICTIONARY else "",
		"methods": methods if typeof(methods) == TYPE_ARRAY else [],
	}


func _accept_request(message: Dictionary) -> void:
	if typeof(message.get("payload")) != TYPE_DICTIONARY:
		_reply(message, CommandQueue.refused("invalid_input", "a request payload must be an object"))
		return
	var refusal: String = queue.enqueue(message)
	if refusal != "":
		_reply(message, CommandQueue.refused("unavailable", refusal))
		return
	_drain_pending = true


func _drain() -> void:
	_drain_pending = false
	queue.drain(_reply)


## Answer `request` with `answer` ({"ok", "payload"} or a refusal). An answer
## that can no longer be delivered, because the connection or the session it
## arrived in ended, is not sent: the server already treats an unanswered
## mutation as unresolved. A mutation's answer that is not delivered, or comes
## after its deadline, when the server may have stopped waiting, is also kept
## for the sessions described afterwards.
func _reply(request: Dictionary, answer: Dictionary) -> void:
	var id := str(request.get("message_id", ""))
	var duration_ms := -1
	var same_session := false
	var received = request.get(RECEIVED_KEY)
	if typeof(received) == TYPE_ARRAY and received.size() >= 2:
		duration_ms = int((Time.get_ticks_usec() - int(received[0])) / 1000.0)
		same_session = int(received[1]) == _session_number
	var mutation := str(request.get("operation_class", "")) == "mutation"
	if _phase != "ready" or not same_session:
		if mutation:
			var kept = _keep_late(request, answer)
			# A session described since the request arrived hears of it now.
			if kept != null and _phase == "ready" and _described:
				_report_late(kept)
		request_answered.emit(request, answer, duration_ms, false)
		return
	var late := mutation and _past_deadline(request)
	var describing := false
	var envelope: Dictionary
	if answer.get("ok", false):
		envelope = _blank("result")
		envelope["correlation_id"] = id
		envelope["message_id"] = "result-%s" % id
		envelope["mutation_status"] = str(answer.get("mutation_status", "not_started"))
		envelope["operation_class"] = "mutation" if mutation else null
		envelope["payload"] = answer.get("payload", {})
		if str(request.get("method", "")) == "bridge.describe_session":
			_described = true
			describing = true
		if _encode(envelope).to_utf8_buffer().size() > MAX_FRAME_BYTES:
			answer = CommandQueue.refusal("internal", "the answer exceeds the bridge frame limit",
				str(answer.get("mutation_status", "not_started")))
	if not answer.get("ok", false):
		envelope = _blank("error")
		envelope["correlation_id"] = id
		envelope["message_id"] = "error-%s" % id
		var message := str(answer.get("message", "the request failed"))
		envelope["error"] = {"kind": str(answer.get("error_kind", "internal")), "message": message.substr(0, 2048)}
		if mutation:
			envelope["operation_class"] = "mutation"
			envelope["mutation_status"] = str(answer.get("mutation_status", "not_started"))
	var delivered := _send(envelope)
	if mutation and (late or not delivered):
		_keep_late(request, answer)
	request_answered.emit(request, answer, duration_ms, delivered)
	if describing and delivered:
		_report_late_answers()


## Tell the server something happened, when it is being served.
func send_event(name: String, payload: Dictionary) -> void:
	if _phase == "ready":
		_send(_event_envelope(name, payload))


# --- late answers ----------------------------------------------------------------


## Whether `request`'s deadline has passed, so its server may have stopped
## waiting for the answer.
static func _past_deadline(request: Dictionary) -> bool:
	var deadline: int = int(str(request.get("deadline_unix_ms", "0")))
	return deadline > 0 and CommandQueue.now_unix_ms() >= deadline


## Keep the mutations among `dropped`, which never started, as answered so.
func _keep_dropped(dropped: Array) -> void:
	for request in dropped:
		if str(request.get("operation_class", "")) == "mutation":
			_keep_late(request, CommandQueue.refusal("unavailable",
				"the connection to the server was lost before the request started"))


## Keep `answer` to the mutation `request` for the sessions described
## afterwards, when it says what happened: an answer that is itself
## unresolved settles nothing. Only a request a server sent for a transaction
## can be settled, so one whose payload names none is not kept. Returns the
## report kept, or null.
func _keep_late(request: Dictionary, answer: Dictionary):
	if not report_late_answers:
		return null
	var status := str(answer.get("mutation_status", "not_started"))
	if status != "committed" and status != "not_started":
		return null
	var received = request.get(RECEIVED_KEY)
	if typeof(received) != TYPE_ARRAY or received.size() < 3 or str(received[2]) == "":
		return null
	var payload = request.get("payload")
	var transaction = payload.get("transaction_id") if typeof(payload) == TYPE_DICTIONARY else null
	if typeof(transaction) != TYPE_STRING or transaction == "":
		return null
	var ok := bool(answer.get("ok", false))
	var reported := {"mutation_status": status, "ok": ok}
	if ok:
		var result = answer.get("payload")
		if typeof(result) == TYPE_DICTIONARY:
			for key in ["action", "digest"]:
				var value = result.get(key)
				if typeof(value) == TYPE_STRING and value != "":
					reported[key] = value.substr(0, MAX_LATE_FIELD_CHARS)
	else:
		reported["error_kind"] = str(answer.get("error_kind", "internal"))
		reported["message"] = str(answer.get("message", "")).substr(0, MAX_LATE_MESSAGE_CHARS)
	var report := {
		"answer": reported,
		"request": {
			"idempotency_id": str(request.get("idempotency_id", "")),
			"message_id": str(request.get("message_id", "")),
			"method": str(request.get("method", "")),
			"session_id": str(received[2]),
			"transaction_id": transaction,
		},
	}
	_late_answers.append(report)
	while _late_answers.size() > MAX_LATE_ANSWERS:
		_late_answers.pop_front()
	return report


## Report every kept late answer to the session just described. A report is
## sent to every later session too: the server ignores one it cannot bind, and
## one whose transaction is settled changes nothing.
func _report_late_answers() -> void:
	for report in _late_answers:
		_report_late(report)


func _report_late(report: Dictionary) -> void:
	_late_reports += 1
	var event := _event_envelope(LATE_ANSWER_EVENT, report)
	event["message_id"] = "event-%s-%d" % [LATE_ANSWER_EVENT, _late_reports]
	_send(event)
