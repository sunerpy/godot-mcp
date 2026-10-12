@tool
extends Node
## The peer end of the godot-mcp bridge: the editor plugin's, and a managed
## game's runtime probe's.
##
## It reads the server's private rendezvous file, dials the loopback endpoint
## it names, proves it read that file by presenting the per-launch secret and
## the project identity in `hello`, and then serves the server's requests
## through the main-thread dispatcher. Everything runs on the main thread: the
## socket is polled from `_process`, and requests are executed by a deferred
## drain. The owner sets `handlers` (the method set it serves),
## `client_version` and, for a peer other than the editor plugin,
## `rendezvous_env` and `rendezvous_path` before adding the client to the tree.
##
## A connection that drops, is refused, or goes silent is closed and dialled
## again from a fresh read of the rendezvous file. Requests still queued when a
## connection is lost are dropped and never executed later, so the server's
## record of an unanswered mutation stays the only one; nothing is replayed.

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

signal status_changed(text: String)
## The connection ended: closed, refused, lost or silent. Nothing queued runs.
signal disconnected(reason: String)

var handlers = null
var queue = null
## The version this peer reports in every envelope's identity.
var client_version := ""
## Where the rendezvous file is: the path in this environment variable, or,
## when it is unset, `rendezvous_path`.
var rendezvous_env := "GODOT_MCP_BRIDGE_RENDEZVOUS"
var rendezvous_path := "res://.godot-mcp/editor-bridge.rendezvous"

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


func _ready() -> void:
	queue = CommandQueue.new(handlers)
	set_process(true)


func _exit_tree() -> void:
	close("the bridge peer is stopping")


func status() -> String:
	return _status


## Whether the server is being served: connected, and the server has had the
## session described, which is when it starts sending work.
func is_connected_to_server() -> bool:
	return _phase == "ready" and _described


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
		queue.drop_all()
	var was_serving := _phase != "idle"
	_phase = "idle"
	if was_serving:
		disconnected.emit(reason)


func _lost(reason: String, retry: float = RETRY_SECONDS) -> void:
	if _peer != null:
		_peer.close(1000, "closing")
		_peer = null
	# Nothing queued may run after the server lost sight of it.
	queue.drop_all()
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
	return JSON.stringify(envelope, "", true, true)


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
			_set_status("connected")
		"error":
			if _phase == "handshaking":
				var body = message.get("error", {})
				_lost("the server refused the handshake (%s)" % str(body.get("kind", "")), REFUSED_RETRY_SECONDS)
		"request":
			if _phase != "ready":
				return
			_accept_request(message)
		"cancel":
			var correlation := str(message.get("correlation_id", ""))
			var withdrawn: Dictionary = queue.cancel(correlation)
			if not withdrawn.is_empty():
				_reply(withdrawn, CommandQueue.refusal("cancelled", "the request was withdrawn before it started"))
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


func _accept_request(message: Dictionary) -> void:
	if typeof(message.get("payload")) != TYPE_DICTIONARY:
		_reply(message, CommandQueue.refusal("invalid_input", "a request payload must be an object"))
		return
	var refusal: String = queue.enqueue(message)
	if refusal != "":
		_reply(message, CommandQueue.refusal("unavailable", refusal))
		return
	if not _drain_pending:
		_drain_pending = true
		_drain.call_deferred()


func _drain() -> void:
	_drain_pending = false
	queue.drain(_reply)


## Answer `request` with `answer` ({"ok", "payload"} or a refusal). An answer
## that can no longer be delivered is dropped: the server already treats an
## unanswered mutation as unresolved.
func _reply(request: Dictionary, answer: Dictionary) -> void:
	if _phase != "ready":
		return
	var mutation := str(request.get("operation_class", "")) == "mutation"
	var id := str(request.get("message_id", ""))
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
	_send(envelope)


## Tell the server something happened, when it is being served.
func send_event(name: String, payload: Dictionary) -> void:
	if _phase == "ready":
		_send(_event_envelope(name, payload))
