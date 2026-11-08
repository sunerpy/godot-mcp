extends SceneTree
## The add-on's project-command caller (`project_commands.gd`), run headless in
## a real Godot against the shipped add-on files: what it refuses before it
## loads anything, and what it reports for a provider that answers, reports an
## error, answers without the documented shape, does not compile, defines no
## mcp_call(), or changed on disk since it was last called.
##
## The add-on lives at res://addons/godot_mcp/; the providers are written under
## res://cases/ as the cases run. Prints one `commands_case ok <name>` or
## `commands_case FAIL <name>: <why>` line per case, then
## `commands_cases: <passed>/<total> passed`, and exits 0 only when every case
## passed. `live_editor_commands.rs` runs it in every Godot build it checks.

const ProjectCommands = preload("res://addons/godot_mcp/project_commands.gd")
const EditorLog = preload("res://addons/godot_mcp/editor_log.gd")

const CASES := [
	"nothing_is_called_on_a_session_that_did_not_offer_commands",
	"a_request_outside_the_bounds_is_refused_before_loading",
	"a_missing_or_plain_script_is_refused_before_loading",
	"a_command_answers_its_value_spelled_by_godot",
	"a_coroutine_is_awaited_and_a_node_provider_is_released",
	"a_command_error_is_reported",
	"an_answer_without_the_shape_is_reported",
	"a_script_that_does_not_compile_is_reported_with_its_log",
	"a_script_without_mcp_call_is_loaded_but_not_called",
	"a_changed_script_is_read_again",
]
const MARKER := "user://commands_ran.txt"
const WRITES_MARKER := "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\tvar file := FileAccess.open(\"user://commands_ran.txt\", FileAccess.WRITE)\n\tfile.store_string(\"ran\")\n\tfile.close()\n\treturn {\"ok\": true, \"value\": 1}\n"
const NO_ARGUMENTS := {"type": "dictionary", "value": {}}

var _failure := ""
var _log = null


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	_log = EditorLog.new()
	_log.start()
	DirAccess.make_dir_recursive_absolute("res://cases")
	var passed := 0
	for name in CASES:
		_failure = ""
		await call(name)
		if _failure == "":
			passed += 1
			print("commands_case ok ", name)
		else:
			print("commands_case FAIL ", name, ": ", _failure)
	_log.stop()
	_log = null
	print("commands_cases: %d/%d passed" % [passed, CASES.size()])
	quit(0 if passed == CASES.size() else 1)


func check(condition: bool, why: String) -> void:
	if not condition and _failure == "":
		_failure = why


## Write `source` as the provider `res://cases/<file>.gd` and return its path.
func provider(file: String, source: String) -> String:
	var path := "res://cases/%s.gd" % file
	var written := FileAccess.open(path, FileAccess.WRITE)
	written.store_string(source)
	written.close()
	return path


## The request the server sends for `command` of the provider at `path`.
func request(path: String, arguments = NO_ARGUMENTS) -> Dictionary:
	return {"provider": "cases", "command": "go", "script": path, "arguments": arguments}


## One call as the editor plugin asks it: offered, logged, nothing to parent.
func offered_call(payload: Dictionary) -> Dictionary:
	return await ProjectCommands.new().call_command(true, payload, _log, null)


func refused(answer: Dictionary, kind: String) -> bool:
	return not answer["ok"] and answer["error_kind"] == kind and answer["mutation_status"] == "not_started"


func nothing_is_called_on_a_session_that_did_not_offer_commands() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(MARKER))
	var path := provider("marker", WRITES_MARKER)
	var plain: Dictionary = await ProjectCommands.new().call_command(false, request(path), _log, null)
	check(refused(plain, "permission_denied"), "a session without the method: %s" % plain)
	check(str(plain["message"]).contains(ProjectCommands.METHOD), "the refusal names the method: %s" % plain)
	check(str(plain["message"]).contains("project commands"), "the refusal says why: %s" % plain)
	check(not FileAccess.file_exists(MARKER), "the command never ran")


func a_request_outside_the_bounds_is_refused_before_loading() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(MARKER))
	var path := provider("bounds", WRITES_MARKER)
	var payloads := [
		{"provider": "", "command": "go", "script": path, "arguments": NO_ARGUMENTS},
		{"provider": 7, "command": "go", "script": path, "arguments": NO_ARGUMENTS},
		{"provider": "cases", "command": "x".repeat(ProjectCommands.MAX_NAME + 1), "script": path, "arguments": NO_ARGUMENTS},
		{"provider": "cases", "command": "go", "script": "res://cases/bounds.txt", "arguments": NO_ARGUMENTS},
		{"provider": "cases", "command": "go", "script": "user://bounds.gd", "arguments": NO_ARGUMENTS},
		{"provider": "cases", "command": "go", "script": "res://cases/../cases/bounds.gd", "arguments": NO_ARGUMENTS},
		{"provider": "cases", "command": "go", "script": "res://.hidden/bounds.gd", "arguments": NO_ARGUMENTS},
		{"provider": "cases", "command": "go", "script": path, "arguments": {"seed": 1}},
		{"provider": "cases", "command": "go", "script": path, "arguments": {"type": "integer", "value": 1}},
		{"provider": "cases", "command": "go", "script": path},
	]
	for payload in payloads:
		var answer: Dictionary = await offered_call(payload)
		check(refused(answer, "invalid_input"), "%s is refused: %s" % [payload, answer])
	check(not FileAccess.file_exists(MARKER), "no refused request ran the command")


func a_missing_or_plain_script_is_refused_before_loading() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(MARKER))
	var missing: Dictionary = await offered_call(request("res://cases/absent.gd"))
	check(refused(missing, "invalid_input"), "a missing script: %s" % missing)
	check(str(missing["message"]).contains("does not exist"), "the refusal says why: %s" % missing)
	var plain_path := provider("plain", WRITES_MARKER.trim_prefix("@tool\n"))
	var plain: Dictionary = await offered_call(request(plain_path))
	check(refused(plain, "invalid_input"), "a plain script: %s" % plain)
	check(str(plain["message"]).contains("@tool"), "the refusal says why: %s" % plain)
	check(not FileAccess.file_exists(MARKER), "a plain script is never loaded or called")


func a_command_answers_its_value_spelled_by_godot() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(MARKER))
	var marker: Dictionary = await offered_call(request(provider("marker", WRITES_MARKER)))
	check(marker["ok"] and marker["mutation_status"] == "committed", "an offered call is answered: %s" % marker)
	check(FileAccess.file_exists(MARKER), "the command ran")
	var path := provider("value", "@tool\nextends RefCounted\n\nfunc mcp_call(command: String, args: Dictionary):\n\tprint(\"called \", command)\n\treturn {\"ok\": true, \"value\": {\"command\": command, \"seed\": args.get(\"seed\")}}\n")
	var answer: Dictionary = await offered_call(request(path, {"type": "dictionary", "value": {"seed": {"type": "integer", "value": 7}}}))
	var report: Dictionary = answer["payload"]
	check(report["stage"] == "done" and report["ran"] and report["ok"], "mcp_call() answered: %s" % report)
	check(report["literal"] == var_to_str({"command": "go", "seed": 7}), "the value is Godot's spelling: %s" % report)
	check(report["source_file"] == path, "the provider's script is named: %s" % report)
	check(report["action"] == "godot-mcp: editor.commands.call", "the action names the method: %s" % report)
	check(report["log_capture"] == _log.capturing(), "the report says whether the log was read")
	if _log.capturing():
		var printed := false
		for entry in report["log"]:
			if entry["kind"] == "message" and str(entry["text"]).contains("called go"):
				printed = true
		check(printed, "what mcp_call() printed is in the log: %s" % [report["log"]])


func a_coroutine_is_awaited_and_a_node_provider_is_released() -> void:
	var waited: Dictionary = await offered_call(request(provider("coroutine", "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\tawait Engine.get_main_loop().process_frame\n\treturn {\"ok\": true, \"value\": 2}\n")))
	check(waited["ok"] and waited["payload"]["literal"] == "2", "the coroutine's value: %s" % waited)
	var parent := Node.new()
	root.add_child(parent)
	var path := provider("node", "@tool\nextends Node\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\tawait get_tree().process_frame\n\treturn {\"ok\": true, \"value\": get_parent() != null and is_inside_tree()}\n")
	var parented: Dictionary = await ProjectCommands.new().call_command(true, request(path), _log, parent)
	check(parented["ok"] and parented["payload"]["literal"] == "true", "a Node provider runs in the tree under its parent: %s" % parented)
	await process_frame
	check(parent.get_child_count() == 0, "the provider's node is released")
	parent.queue_free()


func a_command_error_is_reported() -> void:
	var answer: Dictionary = await offered_call(request(provider("error", "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\treturn {\"ok\": false, \"error\": \"no level is open\"}\n")))
	var report: Dictionary = answer["payload"]
	check(answer["ok"] and answer["mutation_status"] == "committed", "an error is a report, not a refusal: %s" % answer)
	check(report["stage"] == "done" and report["ok"] == false, "the command said it failed: %s" % report)
	check(report["error"] == "no level is open" and report["literal"] == null, "the error text: %s" % report)
	var long: Dictionary = await offered_call(request(provider("long_error", "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\treturn {\"ok\": false, \"error\": \"e\".repeat(5000)}\n")))
	check(str(long["payload"]["error"]).length() == ProjectCommands.MAX_ERROR, "a long error is cut: %s" % str(long["payload"]["error"]).length())


func an_answer_without_the_shape_is_reported() -> void:
	for returned in ["42", "{\"value\": 1}", "{\"ok\": \"yes\"}"]:
		var answer: Dictionary = await offered_call(request(provider("shapeless", "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\treturn %s\n" % returned)))
		var report: Dictionary = answer["payload"]
		check(answer["ok"] and report["stage"] == "returned", "%s is not an answer: %s" % [returned, report])
		check(report["literal"] == var_to_str(str_to_var(returned)), "what it returned is spelled: %s" % report)


func a_script_that_does_not_compile_is_reported_with_its_log() -> void:
	var path := provider("broken_provider", "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\treturn undefined_command_name\n")
	var answer: Dictionary = await offered_call(request(path))
	check(answer["ok"] and answer["mutation_status"] == "committed", "a compile error is a report, not a refusal: %s" % answer)
	var report: Dictionary = answer["payload"]
	check(report["stage"] == "compile" and not report["ran"], "mcp_call() was not called: %s" % report)
	if _log.capturing():
		var at_line := false
		for entry in report["log"]:
			if entry["file"] == path and int(entry["line"]) == 5:
				at_line = true
		check(at_line, "the parse error names line 5 of the provider: %s" % [report["log"]])


func a_script_without_mcp_call_is_loaded_but_not_called() -> void:
	var touched := "user://commands_static.txt"
	DirAccess.remove_absolute(ProjectSettings.globalize_path(touched))
	var answer: Dictionary = await offered_call(request(provider("no_entry", "@tool\nextends RefCounted\n\nstatic var touched := _touch()\n\n\nstatic func _touch() -> int:\n\tvar file := FileAccess.open(\"user://commands_static.txt\", FileAccess.WRITE)\n\tfile.store_string(\"static\")\n\tfile.close()\n\treturn 1\n")))
	var report: Dictionary = answer["payload"]
	check(answer["ok"] and report["stage"] == "provider" and report["ran"], "loaded, not called: %s" % answer)
	check(str(report["reason"]).contains("mcp_call(command, args)"), "the reason: %s" % report)
	check(FileAccess.file_exists(touched), "loading ran the static initializer, as the report says")


func a_changed_script_is_read_again() -> void:
	var source := "@tool\nextends RefCounted\n\nfunc mcp_call(_command: String, _args: Dictionary):\n\treturn {\"ok\": true, \"value\": %d}\n"
	var path := provider("changing", source % 1)
	var first: Dictionary = await offered_call(request(path))
	check(first["ok"] and first["payload"]["literal"] == "1", "the first version: %s" % first)
	provider("changing", source % 2)
	var second: Dictionary = await offered_call(request(path))
	check(second["ok"] and second["payload"]["literal"] == "2", "the file is read again, not the cached copy: %s" % second)
