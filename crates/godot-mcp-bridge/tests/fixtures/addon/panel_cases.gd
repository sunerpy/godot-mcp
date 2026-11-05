extends SceneTree
## The godot_mcp panel's behaviour, run headless in a real Godot against the
## shipped add-on files: what the panel state records for each request the
## editor answered and what it shows of the request's payload, how many rows
## it keeps, the pause switch the dispatcher consults before every mutation,
## the server status it accepts, and what its diagnostics never carry.
##
## The add-on lives at res://addons/godot_mcp/. Prints one
## `panel_case ok <name>` or `panel_case FAIL <name>: <why>` line per case,
## then `panel_cases: <passed>/<total> passed`, and exits 0 only when every
## case passed. The live-editor suite runs it in every Godot build it checks
## (`the_panel_cases_pass_against_the_shipped_add_on`).

const PanelState = preload("res://addons/godot_mcp/panel_state.gd")
const CommandQueue = preload("res://addons/godot_mcp/command_queue.gd")
const BridgeClient = preload("res://addons/godot_mcp/bridge_client.gd")
const Codec = preload("res://addons/godot_mcp/bridge_codec.gd")
const McpPanel = preload("res://addons/godot_mcp/panel.gd")
const StatusDock = preload("res://addons/godot_mcp/status_dock.gd")

const SECRET := "5ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2e75ec2"
const CASES := [
	"targets_name_paths_and_never_values",
	"activity_records_each_answer",
	"activity_keeps_only_the_newest_rows",
	"pause_refuses_mutations_before_they_start",
	"server_status_is_checked_before_it_is_shown",
	"tools_say_what_is_known",
	"the_client_reports_the_session_without_its_secret",
	"reconnect_drops_the_session_and_dials_at_once",
	"an_answer_outliving_its_session_is_not_delivered",
	"copies_are_plain_text",
	"the_panel_draws_what_the_state_holds",
	"the_dock_shows_and_turns_the_pause",
]

var _failure := ""


## A handler set that answers at once and remembers what it ran.
class FakeHandlers:
	extends RefCounted
	var calls: Array = []

	func supports(method: String) -> bool:
		return method in ["test.read", "test.write"]

	func is_mutation(method: String) -> bool:
		return method == "test.write"

	func dispatch(method: String, _payload: Dictionary, _context: Dictionary) -> Dictionary:
		calls.append(method)
		var status := "committed" if method == "test.write" else "not_started"
		return {"ok": true, "payload": {"ran": method}, "mutation_status": status}


## The client with a socket that takes every frame, keeping what was sent.
class RecordingClient:
	extends "res://addons/godot_mcp/bridge_client.gd"
	var sent: Array = []

	func _send(envelope: Dictionary) -> bool:
		sent.append(envelope)
		return true


func _init() -> void:
	var passed := 0
	for name in CASES:
		_failure = ""
		call(name)
		if _failure == "":
			passed += 1
			print("panel_case ok ", name)
		else:
			print("panel_case FAIL ", name, ": ", _failure)
	print("panel_cases: %d/%d passed" % [passed, CASES.size()])
	quit(0 if passed == CASES.size() else 1)


func check(condition: bool, why: String) -> void:
	if not condition and _failure == "":
		_failure = why


static func request(method: String, operation_class: String, payload, id: String = "req-1") -> Dictionary:
	return {
		"message_id": id,
		"method": method,
		"operation_class": operation_class,
		"payload": payload,
		"deadline_unix_ms": "0",
		"idempotency_id": "idem-" + id,
	}


static func answer_ok(status: String = "not_started") -> Dictionary:
	return {"ok": true, "payload": {}, "mutation_status": status}


static func answer_error(kind: String, status: String, message: String = "it failed") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


func targets_name_paths_and_never_values() -> void:
	var node := PanelState.target_of("editor.node", {
		"scene": "res://main.tscn", "node": "Player/Sprite", "properties": ["hidden_property"],
	})
	check(node == "res://main.tscn::Player/Sprite", "editor.node names its scene and node: %s" % node)
	var edited_node := PanelState.target_of("editor.node", {"scene": "", "node": "Missing"})
	check(edited_node == "Missing (edited scene)", "a node of the edited scene: %s" % edited_node)
	var edited := PanelState.target_of("editor.scene.tree", {"scene": ""})
	check(edited == "edited scene", "the edited scene: %s" % edited)
	var picked := PanelState.target_of("editor.selection.set", {"scene": "res://main.tscn", "nodes": ["A", "B"]})
	check(picked == "2 nodes in res://main.tscn", "a selection in a named scene: %s" % picked)
	var one := PanelState.target_of("editor.scene.save", {"scenes": [{"path": "res://a.tscn", "sha256": "f".repeat(64)}], "all": false})
	check(one == "res://a.tscn", "one scene saved: %s" % one)
	var two := PanelState.target_of("editor.scene.save", {"scenes": [{"path": "res://a.tscn"}, {"path": "res://b.tscn"}], "all": true})
	check(two == "2 scenes", "several scenes saved: %s" % two)
	var play := PanelState.target_of("editor.play.start", {"scene": "main", "scenes": [{"path": "res://a.tscn"}]})
	check(play == "main scene", "the scene a run starts: %s" % play)
	var edit := PanelState.target_of("editor.scene.edit", {
		"scene": "res://levels/one.tscn",
		"expected_digest": "d".repeat(64),
		"operations": [{
			"op": "set_property", "node": "Hero", "property": "name",
			"value": {"type": "string", "value": "TOP-SECRET-VALUE"},
		}],
		"label": "LABEL-VALUE",
	})
	check(edit == "res://levels/one.tscn", "an edit names only its scene: %s" % edit)
	var batch := PanelState.target_of("editor.scene.batch", {
		"path": "res://main.tscn", "expected_sha256": "e".repeat(64), "handles": {},
		"operations": [{"op": "add_group", "node": "Hero", "group": "GROUP-VALUE"}],
	})
	check(batch == "res://main.tscn", "a batch names only its scene: %s" % batch)
	var preferences := PanelState.target_of("editor.preferences.set", {
		"settings": {"interface/editor/display_scale": {"type": "integer", "value": 7}},
	})
	check(preferences == "editor settings", "a settings change names no setting or value: %s" % preferences)
	var settings := PanelState.target_of("editor.settings.apply", {
		"changes": [{"key": "application/config/name", "value": "NAME-VALUE"}],
	})
	check(settings == "project.godot", "a project settings change names the file: %s" % settings)
	var status := PanelState.target_of("bridge.server_status", {"server_version": "VERSION-VALUE", "tools": []})
	check(status == "", "the server's own status names nothing: %s" % status)
	var script := PanelState.target_of("editor.script.open", {"path": "res://player.gd", "line": 12, "column": 3})
	check(script == "res://player.gd", "a script names its path: %s" % script)
	var selection := PanelState.target_of("editor.selection.set", {"nodes": ["A", "B", "C"]})
	check(selection == "3 nodes", "a selection counts its nodes: %s" % selection)
	var outside := PanelState.target_of("editor.scene.open", {"path": "/etc/passwd"})
	check(outside == "", "only a res:// path is shown: %s" % outside)
	var typed := PanelState.target_of("editor.scene.open", {"path": 12})
	check(typed == "", "a path that is not a string is not shown: %s" % typed)
	var hostile := PanelState.target_of("editor.scene.open", {"path": "res://evil\u001b[31m\nname.tscn"})
	var control := false
	for index in hostile.length():
		if hostile.unicode_at(index) < 32:
			control = true
	check(not control and hostile.begins_with("res://evil"), "control characters are not shown: %s" % hostile)
	var long := PanelState.target_of("editor.scene.open", {"path": "res://" + "a".repeat(400) + "/end.tscn"})
	check(long.length() <= PanelState.MAX_TARGET_CHARS, "a target is at most %d characters: %d" % [PanelState.MAX_TARGET_CHARS, long.length()])
	check(long.begins_with("res://") and long.ends_with("end.tscn"), "a long path keeps both its ends: %s" % long)
	var none := PanelState.target_of("editor.state", "not a dictionary")
	check(none == "", "a payload that is not an object names nothing")


func activity_records_each_answer() -> void:
	var state = PanelState.new()
	state.record(request("editor.scene.edit", "mutation", {"scene": "res://main.tscn"}), answer_error("conflict", "not_started", "the scene changed"), 12, true)
	var row: Dictionary = state.activity[-1]
	check(row["method"] == "editor.scene.edit", "the method: %s" % row)
	check(row["class"] == "mutation", "the operation class: %s" % row)
	check(row["target"] == "res://main.tscn", "the target: %s" % row)
	check(row["result"] == "conflict", "the error kind: %s" % row)
	check(row["mutation_status"] == "not_started", "the mutation status: %s" % row)
	check(row["duration_ms"] == 12, "the duration: %s" % row)
	check(row["delivered"] == true and row["attention"] == false, "delivered and unremarkable: %s" % row)
	check(int(row["time_unix_ms"]) > 0 and PanelState.clock(int(row["time_unix_ms"])).length() == 8, "a local HH:MM:SS time: %s" % row)
	check(state.failed == 1 and state.served == 0 and state.refused == 0, "a handler's failure counts as failed")

	state.record(request("editor.scene.save", "mutation", {}), answer_error("uncertain", "unresolved", "the save was not proven"), 30, true)
	check(state.activity[-1]["mutation_status"] == "unresolved" and state.activity[-1]["attention"], "an unresolved mutation asks for attention")
	state.record(request("editor.state", "read", {}), answer_ok(), 2, true)
	check(state.activity[-1]["result"] == "ok" and state.activity[-1]["mutation_status"] == "", "a read has no mutation status: %s" % state.activity[-1])
	check(state.served == 1, "an answered read counts as served")
	var refused: Dictionary = CommandQueue.refused("unavailable", "agent edits are paused")
	state.record(request("editor.scene.open", "mutation", {"path": "res://a.tscn"}), refused, 0, true)
	check(state.refused == 1 and state.activity[-1]["refused"], "a refusal before the handler counts as refused")
	state.record(request("editor.selection.set", "mutation", {"nodes": []}), answer_ok("committed"), 4, false)
	check(not state.activity[-1]["delivered"] and state.activity[-1]["attention"], "a mutation whose answer was not delivered asks for attention")
	var served: int = state.served
	state.record(request("bridge.describe_session", "read", {}), answer_ok(), 1, true)
	state.record(request("bridge.server_status", "control", {"tools": []}), answer_ok(), 1, true)
	check(state.served == served, "the session's own bookkeeping is listed but not counted")
	check(state.activity[-1]["class"] == "control", "a control request is listed as control")
	check(state.errors.size() == 3, "the last errors hold every failed answer: %d" % state.errors.size())
	check(state.last_request_unix_ms > 0, "the last request time is known")


func activity_keeps_only_the_newest_rows() -> void:
	var state = PanelState.new()
	for index in PanelState.MAX_ACTIVITY + 100:
		state.record(request("editor.state", "read", {}, "req-%d" % index), answer_ok(), index, true)
	check(state.activity.size() == PanelState.MAX_ACTIVITY, "the ring holds %d rows: %d" % [PanelState.MAX_ACTIVITY, state.activity.size()])
	check(state.dropped == 100, "100 rows were dropped: %d" % state.dropped)
	check(int(state.activity[0]["duration_ms"]) == 100, "the oldest kept row is the 101st: %s" % state.activity[0])
	check(int(state.activity[-1]["duration_ms"]) == PanelState.MAX_ACTIVITY + 99, "the newest row is kept")
	check(state.served == PanelState.MAX_ACTIVITY + 100, "counts are not bounded by the ring: %d" % state.served)
	for index in PanelState.MAX_ERRORS + 5:
		state.record(request("editor.scene.open", "mutation", {}), answer_error("invalid_input", "not_started"), 1, true)
	check(state.errors.size() == PanelState.MAX_ERRORS, "the last errors are bounded: %d" % state.errors.size())
	state.clear_activity()
	check(state.activity.is_empty() and state.dropped == 0, "clearing empties the activity")
	check(state.served == PanelState.MAX_ACTIVITY + 100, "clearing keeps the counts")


func pause_refuses_mutations_before_they_start() -> void:
	var state = PanelState.new()
	var handlers := FakeHandlers.new()
	var queue = CommandQueue.new(handlers)
	queue.mutation_gate = state
	check(not state.mutations_paused(), "edits are allowed until the switch is turned on")
	state.set_paused(true)
	var write: Dictionary = queue.execute(request("test.write", "mutation", {}))
	check(not write["ok"], "a mutation is refused while paused")
	check(write["error_kind"] == "unavailable", "it is refused as unavailable: %s" % write)
	check(write["mutation_status"] == "not_started", "nothing started: %s" % write)
	check(write.get("refused", false), "it is refused before its handler")
	check(str(write["message"]).contains("paused") and str(write["message"]).contains("Godot MCP"), "the message says the user paused edits in the panel: %s" % write)
	check(handlers.calls.is_empty(), "the handler never ran")
	var read: Dictionary = queue.execute(request("test.read", "read", {}))
	check(read["ok"], "a read still runs while paused: %s" % read)
	var control: Dictionary = queue.execute(request("test.read", "control", {}))
	check(control["ok"], "a control request still runs while paused: %s" % control)
	state.set_paused(false)
	var again: Dictionary = queue.execute(request("test.write", "mutation", {}))
	check(again["ok"] and again["mutation_status"] == "committed", "unpaused, the mutation runs: %s" % again)
	check(handlers.calls == ["test.read", "test.read", "test.write"], "the handler ran the read twice and then the write: %s" % [handlers.calls])
	var ungated = CommandQueue.new(handlers)
	var plain: Dictionary = ungated.execute(request("test.write", "mutation", {}))
	check(plain["ok"], "a dispatcher with no switch, the runtime probe's, runs every mutation")


func good_status() -> Dictionary:
	return {
		"server_version": "0.2.0",
		"profile": "full",
		"transport": "stdio",
		"observed_at_unix_ms": 1730000000000.0,
		"mutations_blocked": null,
		"needs": [
			{"needs": "project_files", "component": "project", "status": "ready", "reason": null},
			{"needs": "game_session", "component": "runtime", "status": "unavailable", "reason": "no managed game session is running"},
		],
		"tools_total": 3.0,
		"tools_truncated": false,
		"tools": [
			{"name": "file.get", "title": "Read a file", "needs": "project_files", "read_only": true},
			{"name": "file.patch", "title": "Patch a file", "needs": "project_files", "read_only": false},
			{"name": "runtime.tree.get", "title": "Read the running tree", "needs": "game_session", "read_only": true},
		],
	}


func server_status_is_checked_before_it_is_shown() -> void:
	var state = PanelState.new()
	check(state.accept_server_status(good_status()) == "", "a well-formed status is accepted")
	check(state.server_status["profile"] == "full" and state.server_status["tools"].size() == 3, "it is kept")
	check(state.server_status_ticks_msec >= 0, "the engine time it first arrived is recorded")
	var groups: Array = state.tool_groups()
	check(groups.size() == 2, "the tools are grouped by what they need: %s" % [groups])
	check(groups[0]["needs"] == "project_files" and groups[0]["tools"].size() == 2, "two tools need the project files")
	check(groups[1]["status"] == "unavailable" and str(groups[1]["reason"]).contains("game session"), "a group carries its availability and reason")
	var kept: Dictionary = state.server_status.duplicate(true)
	var broken := good_status()
	broken["tools"] = "not a list"
	check(state.accept_server_status(broken) != "", "tools that are not a list are refused")
	var unnamed := good_status()
	unnamed["tools"] = [{"title": "no name", "needs": "project_files"}]
	check(state.accept_server_status(unnamed) != "", "a tool without a name is refused")
	var many := good_status()
	var tools: Array = []
	for index in PanelState.MAX_TOOLS + 1:
		tools.append({"name": "tool.%d" % index, "needs": "project_files"})
	many["tools"] = tools
	check(state.accept_server_status(many) != "", "more than %d tools are refused" % PanelState.MAX_TOOLS)
	var version := good_status()
	version["server_version"] = 7
	check(state.accept_server_status(version) != "", "a server version that is not a string is refused")
	check(state.server_status == kept, "a refused status leaves the last one in place")
	check(state.server_status_refusal != "", "the refusal is remembered")


func tools_say_what_is_known() -> void:
	var state = PanelState.new()
	check(state.tools_state() == "not_connected", "no session: %s" % state.tools_state())
	state.on_session_opened({"server_version": "0.1.0", "session_id": "s-1", "project_generation": "0", "methods": ["editor.state"]})
	check(state.tools_state() == "not_described", "a server that does not send its status: %s" % state.tools_state())
	state.on_session_opened({"server_version": "0.2.0", "session_id": "s-2", "project_generation": "0", "methods": ["bridge.server_status", "editor.state"]})
	check(state.tools_state() == "waiting", "a server that will send it: %s" % state.tools_state())
	check(state.accept_server_status(good_status()) == "", "accepted")
	check(state.tools_state() == "described", "described: %s" % state.tools_state())
	state.on_disconnected("the connection closed")
	check(state.tools_state() == "not_connected", "after the session ended: %s" % state.tools_state())
	check(state.last_disconnect == "the connection closed", "the reason is kept: %s" % state.last_disconnect)
	state.on_session_opened({"server_version": "0.2.0", "session_id": "s-3", "project_generation": "0", "methods": ["bridge.server_status"]})
	check(state.server_status.is_empty() and state.tools_state() == "waiting", "a new session waits for its own status")


func connected_client(state, client: Node = null) -> Node:
	if client == null:
		client = BridgeClient.new()
	client.handlers = FakeHandlers.new()
	client.queue = CommandQueue.new(client.handlers)
	client.queue.mutation_gate = state
	client.status_changed.connect(state.on_status)
	client.disconnected.connect(state.on_disconnected)
	client.session_opened.connect(state.on_session_opened)
	client.request_answered.connect(state.on_request_answered)
	var claim: Dictionary = Codec.build("capabilities")["project"]
	client._rendezvous = {"endpoint": "127.0.0.1:1", "secret_hex": SECRET, "project": claim}
	client._phase = "handshaking"
	client._on_text(Codec.expected_text("capabilities"))
	return client


func the_client_reports_the_session_without_its_secret() -> void:
	var state = PanelState.new()
	state.set_addon("0.3.0", ["bridge.describe_session"], ["editor.scene.edit"])
	var client: Node = connected_client(state)
	check(state.connected, "the handshake opened a session")
	check(state.server_version == "0.1.0" and state.session_id == "9f2c1d4e6b8a0357", "the server's identity: %s %s" % [state.server_version, state.session_id])
	check(state.project_generation == "7", "the project generation: %s" % state.project_generation)
	check(state.connected_since_unix_ms > 0, "connected since now")
	var envelope: Dictionary = Codec.build("request")
	envelope["deadline_unix_ms"] = int(Time.get_unix_time_from_system() * 1000.0) + 60000
	envelope["method"] = "test.write"
	envelope["operation_class"] = "mutation"
	envelope["payload"] = {"scene": "res://main.tscn", "value": "VALUE-NOT-SHOWN"}
	envelope["message_id"] = "req-write"
	client._on_text(Codec.encode(envelope))
	client._drain()
	check(state.activity.size() == 1, "the answered request is listed: %d" % state.activity.size())
	if not state.activity.is_empty():
		var row: Dictionary = state.activity[-1]
		check(row["method"] == "test.write" and row["result"] == "ok" and row["mutation_status"] == "committed", "its answer: %s" % row)
		check(row["target"] == "res://main.tscn", "its target: %s" % row)
		check(not row["delivered"], "with no socket the answer was not delivered: %s" % row)
		check(int(row["duration_ms"]) >= 0, "its duration from arrival to answer: %s" % row)
	var text: String = state.diagnostics_text()
	check(text.contains("0.1.0") and text.contains("9f2c1d4e6b8a0357") and text.contains("0.3.0"), "the diagnostics name the versions and the session:\n%s" % text)
	check(not text.contains(SECRET), "the diagnostics never carry the rendezvous secret")
	check(not text.contains("VALUE-NOT-SHOWN") and not state.activity_text().contains("VALUE-NOT-SHOWN"), "nor a request's values")
	client.free()


func reconnect_drops_the_session_and_dials_at_once() -> void:
	var state = PanelState.new()
	var client: Node = connected_client(state)
	client._retry_at = 1.0e12
	client.reconnect_now()
	check(client._phase == "idle", "the session is dropped: %s" % client._phase)
	check(client._retry_at <= BridgeClient._now(), "the next frame dials again")
	check(not state.connected, "the panel shows the session ended")
	check(state.last_disconnect.contains("reconnect"), "and why: %s" % state.last_disconnect)
	check(state.connection == "reconnecting", "the status line says so: %s" % state.connection)
	client.free()


static func live_request(id: String, method: String, operation_class: String) -> String:
	var envelope: Dictionary = Codec.build("request")
	envelope["deadline_unix_ms"] = int(Time.get_unix_time_from_system() * 1000.0) + 60000
	envelope["method"] = method
	envelope["operation_class"] = operation_class
	envelope["payload"] = {"scene": "res://main.tscn"}
	envelope["message_id"] = id
	return Codec.encode(envelope)


func an_answer_outliving_its_session_is_not_delivered() -> void:
	var state = PanelState.new()
	var client: Node = connected_client(state, RecordingClient.new())
	client._on_text(live_request("req-running", "test.write", "mutation"))
	client._on_text(live_request("req-queued", "test.read", "read"))
	# The dispatcher started the first; the session ends while it runs, and
	# another opens before it finishes. A restarted server numbers its requests
	# afresh, so one of the new session's has the running request's id.
	var running: Dictionary = client.queue._queue.pop_front()
	client.reconnect_now()
	check(client.queue.size() == 0, "a request queued unrun is dropped: %d" % client.queue.size())
	client._phase = "handshaking"
	client._on_text(Codec.expected_text("capabilities"))
	check(client._phase == "ready" and state.connected, "a new session opened")
	client._on_text(live_request("req-running", "test.read", "read"))
	client.sent.clear()
	client._reply(running, answer_ok("committed"))
	check(client.sent.is_empty(), "the old session's answer is not sent to the new one: %s" % [client.sent])
	var row: Dictionary = state.activity[-1] if not state.activity.is_empty() else {}
	check(row.get("method", "") == "test.write" and not row.get("delivered", true), "the late answer is listed as not delivered: %s" % row)
	check(row.get("attention", false), "a committed edit the server never heard of asks for attention: %s" % row)
	check(int(row.get("duration_ms", -1)) >= 0, "its duration is known: %s" % row)
	client._drain()
	var answered: Dictionary = client.sent[0] if client.sent.size() == 1 else {}
	var payload = answered.get("payload")
	check(answered.get("kind") == "result" and answered.get("correlation_id") == "req-running" and typeof(payload) == TYPE_DICTIONARY and payload.get("ran") == "test.read", "the new session's own request is answered: %s" % [client.sent])
	var last: Dictionary = state.activity[-1] if not state.activity.is_empty() else {}
	check(last.get("method", "") == "test.read" and last.get("delivered", false), "and listed as delivered: %s" % last)
	client.free()


func copies_are_plain_text() -> void:
	var state = PanelState.new()
	state.record(request("editor.scene.open", "mutation", {"path": "res://main.tscn"}), answer_ok("committed"), 7, true)
	var table: String = state.activity_text()
	var lines: PackedStringArray = table.strip_edges().split("\n")
	check(lines.size() == 2, "a header and one row: %s" % table)
	check(lines[0].split("\t").size() == lines[1].split("\t").size(), "every row has the header's columns: %s" % table)
	check(lines[1].contains("editor.scene.open") and lines[1].contains("res://main.tscn") and lines[1].contains("committed"), "the row: %s" % lines[1])
	state.set_paused(true)
	var text: String = state.diagnostics_text()
	check(text.contains("paused"), "the diagnostics say edits are paused:\n%s" % text)


func the_panel_draws_what_the_state_holds() -> void:
	var state = PanelState.new()
	state.set_addon("0.3.0", ["bridge.describe_session", "bridge.server_status", "editor.state"], ["editor.scene.edit"])
	var reconnects := [0]
	var panel: Control = McpPanel.new()
	panel.bind(state, func() -> void: reconnects[0] += 1)
	var shown: Dictionary = panel.snapshot()
	check(shown["tabs"] == ["Status", "Activity", "Tools", "Add-on", "Controls"], "the tabs: %s" % [shown["tabs"]])
	check(str(shown["tools_note"]).contains("Not connected"), "no session: %s" % shown["tools_note"])
	check(shown["addon_methods"] == 4, "the add-on's methods are listed: %s" % shown["addon_methods"])
	state.on_session_opened({"server_version": "0.1.0", "session_id": "s-1", "project_generation": "3", "methods": ["editor.state"]})
	shown = panel.snapshot()
	check(str(shown["tools_note"]).to_lower().contains("the server did not describe its tools"), "an older server: %s" % shown["tools_note"])
	check(shown["tools"] == 0, "no tool is invented: %s" % shown["tools"])
	state.on_session_opened({"server_version": "0.2.0", "session_id": "s-2", "project_generation": "3", "methods": ["bridge.server_status"]})
	shown = panel.snapshot()
	check(str(shown["tools_note"]).contains("Waiting"), "a server that will describe them: %s" % shown["tools_note"])
	check(state.accept_server_status(good_status()) == "", "accepted")
	state.record(request("editor.state", "read", {}), answer_ok(), 3, true)
	state.record(request("editor.scene.save", "mutation", {"scene": "res://main.tscn"}), answer_error("uncertain", "unresolved"), 9, true)
	shown = panel.snapshot()
	check(shown["tool_groups"] == 2 and shown["tools"] == 3, "the tools, grouped: %s" % shown)
	check(shown["activity"].size() == 2, "two rows: %s" % [shown["activity"]])
	if shown["activity"].size() == 2:
		check(shown["activity"][0][1] == "editor.scene.save" and shown["activity"][0][5] == "unresolved", "newest first: %s" % [shown["activity"]])
		check(shown["activity"][0][3] == "res://main.tscn", "with its target: %s" % [shown["activity"]])
	check(shown["status"]["server"] == "0.2.0" and shown["status"]["session"] == "s-2", "the session: %s" % shown["status"])
	check(str(shown["status"]["counts"]).contains("1 served") and str(shown["status"]["counts"]).contains("1 failed"), "the counts: %s" % shown["status"])
	check(str(shown["status"]["profile"]).contains("full"), "the profile: %s" % shown["status"])
	check(not shown["paused"] and shown["status"]["edits"] == "allowed", "edits allowed: %s" % shown)
	check(not shown["paused_banner"], "and no paused banner: %s" % shown)
	state.set_paused(true)
	shown = panel.snapshot()
	check(shown["paused"] and str(shown["status"]["edits"]).contains("PAUSED"), "edits paused: %s" % shown)
	check(shown["paused_banner"], "the panel shows a paused banner: %s" % shown)
	var pause: CheckButton = panel.find_child("PauseAgentEdits", true, false)
	pause.button_pressed = false
	check(not state.mutations_paused(), "the panel's switch turns the pause off")
	var reconnect: Button = panel.find_child("ReconnectNow", true, false)
	reconnect.pressed.emit()
	check(reconnects[0] == 1, "Reconnect now calls the client: %d" % reconnects[0])
	panel.free()


func the_dock_shows_and_turns_the_pause() -> void:
	var state = PanelState.new()
	var reconnects := [0]
	var opened := [0]
	var dock: Control = StatusDock.new()
	dock.bind(state, func() -> void: reconnects[0] += 1, func() -> void: opened[0] += 1)
	state.on_status("connected")
	var shown: Dictionary = dock.snapshot()
	check(shown["status"] == "godot-mcp: connected", "the status line: %s" % shown)
	check(not shown["paused_banner"] and not shown["paused"], "edits allowed: %s" % shown)
	dock.set_paused(true)
	check(state.mutations_paused(), "the dock's switch pauses edits")
	shown = dock.snapshot()
	check(shown["paused_banner"] and shown["paused"], "the dock shows it: %s" % shown)
	state.set_paused(false)
	shown = dock.snapshot()
	check(not shown["paused_banner"] and not shown["paused"], "and that it ended: %s" % shown)
	var reconnect: Button = dock.find_child("ReconnectNow", true, false)
	reconnect.pressed.emit()
	var open: Button = dock.find_child("OpenPanel", true, false)
	open.pressed.emit()
	check(reconnects[0] == 1 and opened[0] == 1, "the buttons call the plugin: %d %d" % [reconnects[0], opened[0]])
	dock.free()
