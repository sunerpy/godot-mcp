@tool
extends SceneTree
## Headless GDScript codec harness for the frozen editor-bridge protocol (v1).
##
## This is the GDScript half of the byte-stability contract. It contains, for every
## one of the ten frozen message kinds:
##
##   * the message rebuilt field by field as a Dictionary, with every key inserted
##     in lexicographic order, and
##   * the canonical text the Rust side produces for that same message, embedded
##     verbatim.
##
## `run_all()` asserts the two agree byte for byte in both directions: the built
## Dictionary must encode to exactly the embedded text, and the embedded text must
## decode and re-encode to the same bytes. If Godot's JSON writer and `serde_json`
## ever disagreed about key order, separators, number formatting or unsigned-integer
## precision, this would fail rather than silently diverging.
##
## Four properties are relied on here. A GDScript `Dictionary` preserves insertion
## order. `JSON.stringify` with its default arguments emits no indentation and no
## spaces after `:` or `,`. `JSON.parse_string` parses every JSON number as
## `TYPE_FLOAT`, which cannot represent the whole u64 range. Therefore `decode`
## locates the protocol's five declared unsigned-integer paths in the original JSON
## text, validates each decimal literal without numeric conversion, and stores its
## exact digits as a String before the rounded float can escape. `encode` finds those
## same paths and writes validated decimal strings back as unquoted number tokens.
## This path-aware text round trip refuses negative, fractional and out-of-range
## values and never converts a received u64 through `float` or GDScript's signed
## 64-bit `int`. Together with keys inserted in sorted order, that reproduces
## `serde_json`'s compact `BTreeMap` rendering for every u64 value through u64::MAX.
##
## Every envelope carries the full fixed key set with inapplicable fields set to
## `null` rather than omitted, which is what makes one shared golden vector
## possible at all: `null` is representable identically on both sides, an absent
## key is not.
##
## Running it, with a real Godot on PATH, from where `assets install` places it
## inside a project (the install root is `godot_mcp/`, not an `addons/` tree,
## because no editor plug-in ships):
##
##     godot --headless --script res://godot_mcp/bridge/bridge_codec.gd
##
## The file is `@tool`-safe, touches no editor API, opens no socket, reads no file
## and needs no scene, so it runs unchanged inside the editor, in a headless run and
## from a unit-test scaffold. Its bytes are pinned by SHA-256 on the Rust side
## (`protocol::harness::GDSCRIPT_HARNESS_SHA256`), so editing it without updating
## that constant is a Rust test failure.

const PROTOCOL_VERSION := "1.0.0"
const SCHEMA_VERSION := "1"
const CANONICAL_VECTOR_OUTPUT_PREFIX := "bridge_codec_vector:"

## Every numeric protocol field is an unsigned integer. Paths are explicit so a
## future field with the same leaf name is not coerced accidentally.
const INTEGER_PATHS := [
	["project", "generation"],
	["deadline_unix_ms"],
	["progress", "completed"],
	["progress", "total"],
	["sent_unix_ms"],
]

const U64_BOUNDARY_LITERALS := [
	"9007199254740992",
	"9007199254740993",
	"18446744073709551615",
]

const U64_MAX_LITERAL := "18446744073709551615"

const U64_PATH_CASES := [
	{"name": "project.generation", "vector": "hello", "needle": "\"generation\":7"},
	{
		"name": "deadline_unix_ms",
		"vector": "request",
		"needle": "\"deadline_unix_ms\":1700000005000",
	},
	{"name": "progress.completed", "vector": "progress", "needle": "\"completed\":3"},
	{"name": "progress.total", "vector": "progress", "needle": "\"total\":7"},
	{
		"name": "sent_unix_ms",
		"vector": "heartbeat",
		"needle": "\"sent_unix_ms\":1700000000000",
	},
]

## Every frozen message kind, in canonical order.
const VECTOR_NAMES := [
	"hello",
	"capabilities",
	"request",
	"result",
	"error",
	"progress",
	"cancel",
	"heartbeat",
	"event",
	"shutdown",
]

## The canonical text the Rust side emits for each vector, embedded verbatim.
static func expected_text(name: String) -> String:
	match name:
		"hello":
			return "{\"auth\":{\"nonce\":\"nonce-0001\",\"secret_hex\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},\"correlation_id\":null,\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":\"4.4.1.stable\",\"plugin_version\":\"0.1.0\",\"server_version\":null,\"session_id\":null},\"kind\":\"hello\",\"message_id\":\"hello-nonce-0001\",\"method\":null,\"mutation_status\":null,\"operation_class\":\"control\",\"origin\":\"plugin\",\"payload\":null,\"progress\":null,\"project\":{\"file_object\":\"unix:dev=66305,ino=4243\",\"generation\":7,\"object_identity\":\"project_1111111111111111111111111111111111111111111111111111111111111111\",\"root_object\":\"unix:dev=66305,ino=4242\"},\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"capabilities":
			return "{\"auth\":null,\"correlation_id\":null,\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":null,\"plugin_version\":null,\"server_version\":\"0.1.0\",\"session_id\":\"9f2c1d4e6b8a0357\"},\"kind\":\"capabilities\",\"message_id\":\"capabilities-9f2c1d4e6b8a0357\",\"method\":null,\"mutation_status\":null,\"operation_class\":\"control\",\"origin\":\"server\",\"payload\":{\"methods\":[\"editor.describe_selection\",\"editor.set_property\"]},\"progress\":null,\"project\":{\"file_object\":\"unix:dev=66305,ino=4243\",\"generation\":7,\"object_identity\":\"project_1111111111111111111111111111111111111111111111111111111111111111\",\"root_object\":\"unix:dev=66305,ino=4242\"},\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"request":
			return "{\"auth\":null,\"correlation_id\":null,\"deadline_unix_ms\":1700000005000,\"error\":null,\"event_name\":null,\"idempotency_id\":\"idem-0001\",\"identity\":{\"godot_version\":null,\"plugin_version\":null,\"server_version\":\"0.1.0\",\"session_id\":\"9f2c1d4e6b8a0357\"},\"kind\":\"request\",\"message_id\":\"req-0001\",\"method\":\"editor.describe_selection\",\"mutation_status\":null,\"operation_class\":\"read\",\"origin\":\"server\",\"payload\":{\"include_children\":true},\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"result":
			return "{\"auth\":null,\"correlation_id\":\"req-0001\",\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":\"4.4.1.stable\",\"plugin_version\":\"0.1.0\",\"server_version\":null,\"session_id\":null},\"kind\":\"result\",\"message_id\":\"result-req-0001\",\"method\":null,\"mutation_status\":\"not_started\",\"operation_class\":null,\"origin\":\"plugin\",\"payload\":{\"selected\":[\"res://main.tscn::Player\"]},\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"error":
			return "{\"auth\":null,\"correlation_id\":\"req-0002\",\"deadline_unix_ms\":null,\"error\":{\"kind\":\"cancelled\",\"message\":\"the request was cancelled before it started\"},\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":\"4.4.1.stable\",\"plugin_version\":\"0.1.0\",\"server_version\":null,\"session_id\":null},\"kind\":\"error\",\"message_id\":\"error-req-0002\",\"method\":null,\"mutation_status\":null,\"operation_class\":null,\"origin\":\"plugin\",\"payload\":null,\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"progress":
			return "{\"auth\":null,\"correlation_id\":\"req-0003\",\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":\"4.4.1.stable\",\"plugin_version\":\"0.1.0\",\"server_version\":null,\"session_id\":null},\"kind\":\"progress\",\"message_id\":\"progress-req-0003-3\",\"method\":null,\"mutation_status\":null,\"operation_class\":null,\"origin\":\"plugin\",\"payload\":null,\"progress\":{\"completed\":3,\"note\":\"importing resources\",\"total\":7},\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"cancel":
			return "{\"auth\":null,\"correlation_id\":\"req-0002\",\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":null,\"plugin_version\":null,\"server_version\":\"0.1.0\",\"session_id\":\"9f2c1d4e6b8a0357\"},\"kind\":\"cancel\",\"message_id\":\"cnl-0001\",\"method\":null,\"mutation_status\":null,\"operation_class\":\"control\",\"origin\":\"server\",\"payload\":null,\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"heartbeat":
			return "{\"auth\":null,\"correlation_id\":null,\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":null,\"plugin_version\":null,\"server_version\":\"0.1.0\",\"session_id\":\"9f2c1d4e6b8a0357\"},\"kind\":\"heartbeat\",\"message_id\":\"hb-0001\",\"method\":null,\"mutation_status\":null,\"operation_class\":\"control\",\"origin\":\"server\",\"payload\":null,\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":1700000000000}"
		"event":
			return "{\"auth\":null,\"correlation_id\":null,\"deadline_unix_ms\":null,\"error\":null,\"event_name\":\"editor.scene_changed\",\"idempotency_id\":null,\"identity\":{\"godot_version\":\"4.4.1.stable\",\"plugin_version\":\"0.1.0\",\"server_version\":null,\"session_id\":null},\"kind\":\"event\",\"message_id\":\"evt-0001\",\"method\":null,\"mutation_status\":null,\"operation_class\":null,\"origin\":\"plugin\",\"payload\":{\"scene\":\"res://main.tscn\"},\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":null,\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		"shutdown":
			return "{\"auth\":null,\"correlation_id\":null,\"deadline_unix_ms\":null,\"error\":null,\"event_name\":null,\"idempotency_id\":null,\"identity\":{\"godot_version\":null,\"plugin_version\":null,\"server_version\":\"0.1.0\",\"session_id\":\"9f2c1d4e6b8a0357\"},\"kind\":\"shutdown\",\"message_id\":\"sd-0001\",\"method\":null,\"mutation_status\":null,\"operation_class\":\"control\",\"origin\":\"server\",\"payload\":null,\"progress\":null,\"project\":null,\"protocol_version\":\"1.0.0\",\"reason\":\"server is stopping\",\"schema_version\":\"1\",\"sent_unix_ms\":null}"
		_:
			return ""


## The same messages rebuilt field by field, keys inserted in sorted order.
static func build(name: String) -> Dictionary:
	match name:
		"hello":
			return {
				"auth": {
					"nonce": "nonce-0001",
					"secret_hex": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
				},
				"correlation_id": null,
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": "4.4.1.stable",
					"plugin_version": "0.1.0",
					"server_version": null,
					"session_id": null,
				},
				"kind": "hello",
				"message_id": "hello-nonce-0001",
				"method": null,
				"mutation_status": null,
				"operation_class": "control",
				"origin": "plugin",
				"payload": null,
				"progress": null,
				"project": {
					"file_object": "unix:dev=66305,ino=4243",
					"generation": 7,
					"object_identity": "project_1111111111111111111111111111111111111111111111111111111111111111",
					"root_object": "unix:dev=66305,ino=4242",
				},
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"capabilities":
			return {
				"auth": null,
				"correlation_id": null,
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": null,
					"plugin_version": null,
					"server_version": "0.1.0",
					"session_id": "9f2c1d4e6b8a0357",
				},
				"kind": "capabilities",
				"message_id": "capabilities-9f2c1d4e6b8a0357",
				"method": null,
				"mutation_status": null,
				"operation_class": "control",
				"origin": "server",
				"payload": {
					"methods": ["editor.describe_selection", "editor.set_property"],
				},
				"progress": null,
				"project": {
					"file_object": "unix:dev=66305,ino=4243",
					"generation": 7,
					"object_identity": "project_1111111111111111111111111111111111111111111111111111111111111111",
					"root_object": "unix:dev=66305,ino=4242",
				},
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"request":
			return {
				"auth": null,
				"correlation_id": null,
				"deadline_unix_ms": 1700000005000,
				"error": null,
				"event_name": null,
				"idempotency_id": "idem-0001",
				"identity": {
					"godot_version": null,
					"plugin_version": null,
					"server_version": "0.1.0",
					"session_id": "9f2c1d4e6b8a0357",
				},
				"kind": "request",
				"message_id": "req-0001",
				"method": "editor.describe_selection",
				"mutation_status": null,
				"operation_class": "read",
				"origin": "server",
				"payload": {
					"include_children": true,
				},
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"result":
			return {
				"auth": null,
				"correlation_id": "req-0001",
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": "4.4.1.stable",
					"plugin_version": "0.1.0",
					"server_version": null,
					"session_id": null,
				},
				"kind": "result",
				"message_id": "result-req-0001",
				"method": null,
				"mutation_status": "not_started",
				"operation_class": null,
				"origin": "plugin",
				"payload": {
					"selected": ["res://main.tscn::Player"],
				},
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"error":
			return {
				"auth": null,
				"correlation_id": "req-0002",
				"deadline_unix_ms": null,
				"error": {
					"kind": "cancelled",
					"message": "the request was cancelled before it started",
				},
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": "4.4.1.stable",
					"plugin_version": "0.1.0",
					"server_version": null,
					"session_id": null,
				},
				"kind": "error",
				"message_id": "error-req-0002",
				"method": null,
				"mutation_status": null,
				"operation_class": null,
				"origin": "plugin",
				"payload": null,
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"progress":
			return {
				"auth": null,
				"correlation_id": "req-0003",
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": "4.4.1.stable",
					"plugin_version": "0.1.0",
					"server_version": null,
					"session_id": null,
				},
				"kind": "progress",
				"message_id": "progress-req-0003-3",
				"method": null,
				"mutation_status": null,
				"operation_class": null,
				"origin": "plugin",
				"payload": null,
				"progress": {
					"completed": 3,
					"note": "importing resources",
					"total": 7,
				},
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"cancel":
			return {
				"auth": null,
				"correlation_id": "req-0002",
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": null,
					"plugin_version": null,
					"server_version": "0.1.0",
					"session_id": "9f2c1d4e6b8a0357",
				},
				"kind": "cancel",
				"message_id": "cnl-0001",
				"method": null,
				"mutation_status": null,
				"operation_class": "control",
				"origin": "server",
				"payload": null,
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"heartbeat":
			return {
				"auth": null,
				"correlation_id": null,
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": null,
					"plugin_version": null,
					"server_version": "0.1.0",
					"session_id": "9f2c1d4e6b8a0357",
				},
				"kind": "heartbeat",
				"message_id": "hb-0001",
				"method": null,
				"mutation_status": null,
				"operation_class": "control",
				"origin": "server",
				"payload": null,
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": 1700000000000,
			}
		"event":
			return {
				"auth": null,
				"correlation_id": null,
				"deadline_unix_ms": null,
				"error": null,
				"event_name": "editor.scene_changed",
				"idempotency_id": null,
				"identity": {
					"godot_version": "4.4.1.stable",
					"plugin_version": "0.1.0",
					"server_version": null,
					"session_id": null,
				},
				"kind": "event",
				"message_id": "evt-0001",
				"method": null,
				"mutation_status": null,
				"operation_class": null,
				"origin": "plugin",
				"payload": {
					"scene": "res://main.tscn",
				},
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": null,
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		"shutdown":
			return {
				"auth": null,
				"correlation_id": null,
				"deadline_unix_ms": null,
				"error": null,
				"event_name": null,
				"idempotency_id": null,
				"identity": {
					"godot_version": null,
					"plugin_version": null,
					"server_version": "0.1.0",
					"session_id": "9f2c1d4e6b8a0357",
				},
				"kind": "shutdown",
				"message_id": "sd-0001",
				"method": null,
				"mutation_status": null,
				"operation_class": "control",
				"origin": "server",
				"payload": null,
				"progress": null,
				"project": null,
				"protocol_version": "1.0.0",
				"reason": "server is stopping",
				"schema_version": "1",
				"sent_unix_ms": null,
			}
		_:
			return {}


static func skip_json_whitespace(text: String, start: int) -> int:
	var cursor := start
	while cursor < text.length():
		var code := text.unicode_at(cursor)
		if code != 9 and code != 10 and code != 13 and code != 32:
			break
		cursor += 1
	return cursor


static func scan_json_string_end(text: String, start: int) -> int:
	if start >= text.length() or text.unicode_at(start) != 34:
		return -1
	var cursor := start + 1
	while cursor < text.length():
		var code := text.unicode_at(cursor)
		if code == 34:
			return cursor + 1
		if code == 92:
			cursor += 1
			if cursor >= text.length():
				return -1
		cursor += 1
	return -1


static func scan_json_value_end(text: String, start: int) -> int:
	var cursor := skip_json_whitespace(text, start)
	if cursor >= text.length():
		return -1
	var first := text.unicode_at(cursor)
	if first == 34:
		return scan_json_string_end(text, cursor)
	if first == 123 or first == 91:
		var closers: Array[int] = [125 if first == 123 else 93]
		cursor += 1
		while cursor < text.length():
			var code := text.unicode_at(cursor)
			if code == 34:
				cursor = scan_json_string_end(text, cursor)
				if cursor < 0:
					return -1
				continue
			if code == 123:
				closers.append(125)
			elif code == 91:
				closers.append(93)
			elif code == 125 or code == 93:
				if closers.is_empty() or closers.back() != code:
					return -1
				closers.pop_back()
				cursor += 1
				if closers.is_empty():
					return cursor
				continue
			cursor += 1
		return -1
	while cursor < text.length():
		var code := text.unicode_at(cursor)
		if code == 9 or code == 10 or code == 13 or code == 32 or code == 44 or code == 93 or code == 125:
			break
		cursor += 1
	return cursor if cursor > start else -1


static func find_object_member_span(text: String, object_start: int, target: String) -> Dictionary:
	var cursor := skip_json_whitespace(text, object_start)
	if cursor >= text.length() or text.unicode_at(cursor) != 123:
		return {"ok": false}
	cursor += 1
	var found := false
	var found_start := -1
	var found_end := -1
	while cursor < text.length():
		cursor = skip_json_whitespace(text, cursor)
		if cursor >= text.length():
			return {"ok": false}
		if text.unicode_at(cursor) == 125:
			return {
				"ok": true,
				"found": found,
				"start": found_start,
				"end": found_end,
			}
		var key_start := cursor
		var key_end := scan_json_string_end(text, key_start)
		if key_end < 0:
			return {"ok": false}
		var key_value: Variant = JSON.parse_string(text.substr(key_start, key_end - key_start))
		if typeof(key_value) != TYPE_STRING:
			return {"ok": false}
		cursor = skip_json_whitespace(text, key_end)
		if cursor >= text.length() or text.unicode_at(cursor) != 58:
			return {"ok": false}
		var value_start := skip_json_whitespace(text, cursor + 1)
		var value_end := scan_json_value_end(text, value_start)
		if value_end < 0:
			return {"ok": false}
		if String(key_value) == target:
			if found:
				return {"ok": false}
			found = true
			found_start = value_start
			found_end = value_end
		cursor = skip_json_whitespace(text, value_end)
		if cursor >= text.length():
			return {"ok": false}
		var delimiter := text.unicode_at(cursor)
		if delimiter == 44:
			cursor += 1
		elif delimiter != 125:
			return {"ok": false}
	return {"ok": false}


static func raw_value_at_path(text: String, path: Array) -> Dictionary:
	var current_start := skip_json_whitespace(text, 0)
	var current_end := scan_json_value_end(text, current_start)
	if current_end < 0 or skip_json_whitespace(text, current_end) != text.length():
		return {"ok": false}
	for segment_value: Variant in path:
		var current := text.substr(current_start, current_end - current_start)
		if current == "null":
			return {"ok": true, "found": false}
		var member := find_object_member_span(text, current_start, String(segment_value))
		if not member.get("ok", false):
			return {"ok": false}
		if not member.get("found", false):
			return {"ok": true, "found": false}
		current_start = int(member["start"])
		current_end = int(member["end"])
	return {"ok": true, "found": true, "start": current_start, "end": current_end}


static func is_u64_literal(literal: String) -> bool:
	if literal.is_empty() or (literal.length() > 1 and literal.unicode_at(0) == 48):
		return false
	for index: int in range(literal.length()):
		var code := literal.unicode_at(index)
		if code < 48 or code > 57:
			return false
	if literal.length() != U64_MAX_LITERAL.length():
		return literal.length() < U64_MAX_LITERAL.length()
	for index: int in range(literal.length()):
		var code := literal.unicode_at(index)
		var maximum_code := U64_MAX_LITERAL.unicode_at(index)
		if code != maximum_code:
			return code < maximum_code
	return true


static func set_integer_path_literal(envelope: Dictionary, path: Array, literal: String) -> bool:
	var container := envelope
	for index: int in range(path.size() - 1):
		var nested: Variant = container.get(path[index])
		if typeof(nested) != TYPE_DICTIONARY:
			return false
		container = nested
	var field := String(path[path.size() - 1])
	if not container.has(field):
		return false
	container[field] = literal
	return true


## Encode one message as compact JSON. Decimal strings returned by `decode` at
## declared u64 paths are reinserted as unquoted numeric literals, from right to
## left so replacement offsets remain valid. Invalid u64 text returns "".
static func encode(envelope: Dictionary) -> String:
	var text := JSON.stringify(envelope)
	var replacements: Array[Dictionary] = []
	for path: Array in INTEGER_PATHS:
		var span := raw_value_at_path(text, path)
		if not span.get("ok", false):
			return ""
		if not span.get("found", false):
			continue
		var start := int(span["start"])
		var end := int(span["end"])
		var token := text.substr(start, end - start)
		if token == "null":
			continue
		if token.begins_with("\""):
			var value: Variant = JSON.parse_string(token)
			if typeof(value) != TYPE_STRING or not is_u64_literal(String(value)):
				return ""
			replacements.append({"start": start, "end": end, "literal": String(value)})
		elif not is_u64_literal(token):
			return ""
	while not replacements.is_empty():
		var latest_index := 0
		for index: int in range(1, replacements.size()):
			if int(replacements[index]["start"]) > int(replacements[latest_index]["start"]):
				latest_index = index
		var replacement := replacements[latest_index]
		var start := int(replacement["start"])
		var end := int(replacement["end"])
		text = text.substr(0, start) + String(replacement["literal"]) + text.substr(end)
		replacements.remove_at(latest_index)
	return text


## Decode one received message. Every declared u64 path is overwritten with its
## validated raw decimal literal before the rounded TYPE_FLOAT can escape. An empty
## Dictionary names malformed JSON, a duplicate path, or invalid u64 text.
static func decode(text: String) -> Dictionary:
	var parsed_value: Variant = JSON.parse_string(text)
	if typeof(parsed_value) != TYPE_DICTIONARY:
		return {}
	var parsed: Dictionary = parsed_value
	for path: Array in INTEGER_PATHS:
		var span := raw_value_at_path(text, path)
		if not span.get("ok", false):
			return {}
		if not span.get("found", false):
			continue
		var literal := text.substr(int(span["start"]), int(span["end"]) - int(span["start"]))
		if literal == "null":
			continue
		if not is_u64_literal(literal) or not set_integer_path_literal(parsed, path, literal):
			return {}
	return parsed


## Check one vector in both directions. Returns "" on success, or the failure.
static func check(name: String) -> String:
	var expected: String = expected_text(name)
	if expected == "":
		return "%s: no embedded golden text" % name
	var built: Dictionary = build(name)
	if built.is_empty():
		return "%s: no rebuilt message" % name
	var encoded: String = encode(built)
	if encoded != expected:
		return "%s: encode mismatch\n  built:    %s\n  expected: %s" % [name, encoded, expected]
	print(
		CANONICAL_VECTOR_OUTPUT_PREFIX
		+ JSON.stringify({"name": name, "canonical_json": encoded})
	)
	var decoded: Dictionary = decode(expected)
	if decoded.is_empty():
		return "%s: the golden text did not decode to an object" % name
	if encode(decoded) != expected:
		return "%s: decode/re-encode is not byte-stable" % name
	if decoded.size() != built.size():
		return "%s: decoded key count %d != built key count %d" % [name, decoded.size(), built.size()]
	for key: String in built.keys():
		if not decoded.has(key):
			return "%s: decoded message lacks key %s" % [name, key]
	return ""


## Check the frame limit the protocol freezes, without allocating it.
static func frame_limit_bytes() -> int:
	return 4 * 1024 * 1024


## Whether a message of `byte_length` may be sent at all.
static func within_frame_limit(byte_length: int) -> bool:
	return byte_length <= frame_limit_bytes()


## Every integer-valued protocol path must reject a genuinely fractional value.
static func check_fractional_integer_refusal() -> Array:
	var cases: Dictionary = {}
	var hello: Dictionary = build("hello")
	hello["project"]["generation"] = 7.5
	cases["project.generation"] = JSON.stringify(hello)
	var request: Dictionary = build("request")
	request["deadline_unix_ms"] = 1700000005000.5
	cases["deadline_unix_ms"] = JSON.stringify(request)
	var progress_completed: Dictionary = build("progress")
	progress_completed["progress"]["completed"] = 3.5
	cases["progress.completed"] = JSON.stringify(progress_completed)
	var progress_total: Dictionary = build("progress")
	progress_total["progress"]["total"] = 7.5
	cases["progress.total"] = JSON.stringify(progress_total)
	var heartbeat: Dictionary = build("heartbeat")
	heartbeat["sent_unix_ms"] = 1700000000000.5
	cases["sent_unix_ms"] = JSON.stringify(heartbeat)

	var failures: Array = []
	for path: String in cases:
		if not decode(cases[path]).is_empty():
			failures.append("%s: fractional integer was not refused" % path)
	return failures


## The three precision boundaries must survive through the real Godot codec as
## byte-identical decimal literals, including the half of u64 outside TYPE_INT.
static func check_u64_boundaries() -> Array:
	var failures: Array = []
	for case: Dictionary in U64_PATH_CASES:
		for literal: String in U64_BOUNDARY_LITERALS:
			var expected: String = expected_text(case["vector"]).replace(
				case["needle"], "%s:%s" % [String(case["needle"]).get_slice(":", 0), literal]
			)
			var decoded: Dictionary = decode(expected)
			var actual: String = encode(decoded)
			if actual != expected:
				failures.append(
					"%s u64 %s: decode/re-encode changed bytes to %s"
					% [case["name"], literal, actual]
				)
	return failures


## Check every vector. Returns an empty Array on success, or one string per
## failure.
static func run_all() -> Array:
	var failures: Array = []
	for name: String in VECTOR_NAMES:
		var failure: String = check(name)
		if failure != "":
			failures.append(failure)
	if not within_frame_limit(frame_limit_bytes()):
		failures.append("the frame limit rejects a message of exactly its own size")
	if within_frame_limit(frame_limit_bytes() + 1):
		failures.append("the frame limit accepts an oversize message")
	if within_frame_limit(16 * 1024 * 1024):
		failures.append("the frame limit accepts a 16 MiB message")
	failures.append_array(check_fractional_integer_refusal())
	failures.append_array(check_u64_boundaries())
	return failures


func _init() -> void:
	var failures: Array = run_all()
	for failure: String in failures:
		printerr(failure)
	if failures.is_empty():
		print(
			"bridge_codec: %d/%d vectors byte-identical; %d/%d fractional integer cases refused; %d/%d u64 path/boundary cases byte-identical"
			% [
				VECTOR_NAMES.size(),
				VECTOR_NAMES.size(),
				INTEGER_PATHS.size(),
				INTEGER_PATHS.size(),
				U64_BOUNDARY_LITERALS.size() * U64_PATH_CASES.size(),
				U64_BOUNDARY_LITERALS.size() * U64_PATH_CASES.size(),
			]
		)
		quit(0)
	else:
		printerr("bridge_codec: %d failure(s)" % failures.size())
		quit(1)
