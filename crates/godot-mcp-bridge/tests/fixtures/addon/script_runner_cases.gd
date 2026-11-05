extends SceneTree
## The add-on's script runner (`script_runner.gd`), run headless in a real
## Godot against the shipped add-on files: which sources it marks as tool
## scripts, what it refuses before compiling anything, and what it reports for
## a source that returns, does not compile, or defines no run().
##
## The add-on lives at res://addons/godot_mcp/. Prints one
## `runner_case ok <name>` or `runner_case FAIL <name>: <why>` line per case,
## then `runner_cases: <passed>/<total> passed`, and exits 0 only when every
## case passed. `live_editor_script.rs` runs it in every Godot build it checks.

const ScriptRunner = preload("res://addons/godot_mcp/script_runner.gd")
const EditorLog = preload("res://addons/godot_mcp/editor_log.gd")

const CASES := [
	"tool_is_written_only_when_the_source_lacks_it",
	"nothing_runs_on_a_session_that_did_not_offer_scripts",
	"a_source_outside_the_bounds_is_refused",
	"a_returned_value_is_spelled_by_godot",
	"a_coroutine_is_awaited",
	"a_source_that_does_not_compile_is_reported_with_its_log",
	"a_source_without_run_is_compiled_but_not_called",
]
const MARKER := "user://runner_ran.txt"
const WRITES_MARKER := "extends RefCounted\n\nfunc run():\n\tvar file := FileAccess.open(\"user://runner_ran.txt\", FileAccess.WRITE)\n\tfile.store_string(\"ran\")\n\tfile.close()\n\treturn 1\n"

var _failure := ""
var _log = null


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	_log = EditorLog.new()
	_log.start()
	var passed := 0
	for name in CASES:
		_failure = ""
		await call(name)
		if _failure == "":
			passed += 1
			print("runner_case ok ", name)
		else:
			print("runner_case FAIL ", name, ": ", _failure)
	_log.stop()
	_log = null
	print("runner_cases: %d/%d passed" % [passed, CASES.size()])
	quit(0 if passed == CASES.size() else 1)


func check(condition: bool, why: String) -> void:
	if not condition and _failure == "":
		_failure = why


## One run as the editor plugin asks it: offered, logged, nothing to parent.
func offered_run(payload: Dictionary) -> Dictionary:
	return await ScriptRunner.new().run(ScriptRunner.EDITOR_METHOD, true, payload, _log, null)


func tool_is_written_only_when_the_source_lacks_it() -> void:
	check(ScriptRunner.as_tool("extends Node\n") == "@tool extends Node\n", "a plain source gains @tool on its first line")
	check(ScriptRunner.as_tool("@tool\nextends Node\n") == "@tool\nextends Node\n", "a tool source is unchanged")
	check(ScriptRunner.as_tool("# a note\n  @tool\nextends Node\n") == "# a note\n  @tool\nextends Node\n", "@tool after a comment counts")
	check(ScriptRunner.as_tool("@toolbox\n").begins_with("@tool @toolbox"), "a longer annotation is not @tool")
	var script := GDScript.new()
	script.source_code = ScriptRunner.as_tool("extends RefCounted\n\nfunc run():\n\treturn 1\n")
	check(script.reload() == OK and script.is_tool(), "the marked source compiles as a tool script")


func nothing_runs_on_a_session_that_did_not_offer_scripts() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(MARKER))
	var runner = ScriptRunner.new()
	var plain: Dictionary = await runner.run(ScriptRunner.RUNTIME_METHOD, false, {"source": WRITES_MARKER}, _log, null)
	check(not plain["ok"] and plain["error_kind"] == "permission_denied", "a session without the method: %s" % plain)
	check(str(plain["message"]).contains(ScriptRunner.RUNTIME_METHOD), "the refusal names the method: %s" % plain)
	check(plain["mutation_status"] == "not_started", "a refusal starts nothing: %s" % plain)
	check(str(plain["message"]).contains("script execution"), "the refusal says why: %s" % plain)
	check(not FileAccess.file_exists(MARKER), "the source never ran")


func a_source_outside_the_bounds_is_refused() -> void:
	for source in ["", "x".repeat(ScriptRunner.MAX_SOURCE + 1), 7]:
		var answer: Dictionary = await offered_run({"source": source})
		check(not answer["ok"] and answer["error_kind"] == "invalid_input", "%s is refused: %s" % [typeof(source), answer])


func a_returned_value_is_spelled_by_godot() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(MARKER))
	var answer: Dictionary = await offered_run({"source": WRITES_MARKER})
	check(answer["ok"] and answer["mutation_status"] == "committed", "an offered run is answered: %s" % answer)
	check(FileAccess.file_exists(MARKER), "the source ran")
	var valued: Dictionary = await offered_run({"source": "extends RefCounted\n\nfunc run():\n\tprint(\"hello from run\")\n\treturn {\"a\": 1}\n"})
	var report: Dictionary = valued["payload"]
	check(report["stage"] == "done" and report["ran"], "run() returned: %s" % report)
	check(report["literal"] == var_to_str({"a": 1}), "the value is Godot's spelling: %s" % report)
	check(str(report["source_file"]).begins_with("gdscript://"), "the compiled source is named: %s" % report)
	check(report["log_capture"] == _log.capturing(), "the report says whether the log was read")
	if _log.capturing():
		var printed := false
		for entry in report["log"]:
			if entry["kind"] == "message" and str(entry["text"]).contains("hello from run"):
				printed = true
		check(printed, "what run() printed is in the log: %s" % report["log"])
	var node: Dictionary = await offered_run({"source": "extends Node\n\nfunc run():\n\treturn name.is_empty()\n"})
	check(node["ok"] and node["payload"]["literal"] == "true", "a Node source runs: %s" % node)
	var parent := Node.new()
	root.add_child(parent)
	var parented: Dictionary = await ScriptRunner.new().run(ScriptRunner.RUNTIME_METHOD, true, {"source": "extends Node\n\nfunc run():\n\tawait get_tree().process_frame\n\treturn get_parent() != null and is_inside_tree()\n"}, _log, parent)
	check(parented["ok"] and parented["payload"]["literal"] == "true", "a Node source runs in the tree under its parent: %s" % parented)
	check(parented["payload"]["action"] == "godot-mcp: runtime.script.run", "the action names the method: %s" % parented)
	await process_frame
	check(parent.get_child_count() == 0, "the run's node is released")
	parent.queue_free()


func a_coroutine_is_awaited() -> void:
	var answer: Dictionary = await offered_run({"source": "extends RefCounted\n\nfunc run():\n\tawait Engine.get_main_loop().process_frame\n\treturn 2\n"})
	check(answer["ok"] and answer["payload"]["literal"] == "2", "the coroutine's value: %s" % answer)


func a_source_that_does_not_compile_is_reported_with_its_log() -> void:
	var answer: Dictionary = await offered_run({"source": "extends RefCounted\n\nfunc run():\n\treturn undefined_name\n"})
	check(answer["ok"], "a compile error is a report, not a refusal: %s" % answer)
	var report: Dictionary = answer["payload"]
	check(report["stage"] == "compile" and not report["ran"], "nothing of the source ran: %s" % report)
	check(str(report["reason"]).contains("does not compile"), "the reason: %s" % report)
	if _log.capturing():
		var at_line := false
		for entry in report["log"]:
			if entry["file"] == report["source_file"] and int(entry["line"]) == 4:
				at_line = true
		check(at_line, "the parse error names line 4 of the source: %s" % report["log"])


func a_source_without_run_is_compiled_but_not_called() -> void:
	var touched := "user://runner_static.txt"
	DirAccess.remove_absolute(ProjectSettings.globalize_path(touched))
	var answer: Dictionary = await offered_run({"source": "extends RefCounted\n\nstatic var touched := _touch()\n\n\nstatic func _touch() -> int:\n\tvar file := FileAccess.open(\"user://runner_static.txt\", FileAccess.WRITE)\n\tfile.store_string(\"static\")\n\tfile.close()\n\treturn 1\n"})
	var report: Dictionary = answer["payload"]
	check(answer["ok"] and report["stage"] == "compile" and report["ran"], "compiled, not called: %s" % answer)
	check(str(report["reason"]).contains("no run()"), "the reason: %s" % report)
	check(FileAccess.file_exists(touched), "compiling ran the static initializer, as the report says")
