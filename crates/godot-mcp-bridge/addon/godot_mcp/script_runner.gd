@tool
extends RefCounted
## `editor.script.run` and `runtime.script.run`: compile caller-written
## GDScript in the editor, or in a game through its runtime probe, and call its
## `run()`, awaiting it when it is a coroutine.
##
## Each method is offered only by a server whose operator turned script
## execution on: such a server lists it in the session's capabilities, and a
## request on a session whose capabilities did not list it is refused here
## before anything is compiled.
##
## The source runs on the process's main thread, with the user's privileges,
## and nothing confines what it does. It runs as a tool script, so the editor
## runs it too: `@tool` is written in front of its first line when the source
## does not say so itself, so its line numbers stay those of the caller's
## source. Compiling it runs its static initializers. Godot cannot stop a
## script part way through: a run that never returns keeps the process busy,
## and only stopping the editor or the game ends it.
##
## The answer says how far the run got (`stage`), what `run()` returned,
## spelled by Godot's own `var_to_str`, and what was logged while the source
## compiled and ran, on a Godot whose log a script can read (4.5 and later).

const EDITOR_METHOD := "editor.script.run"
const RUNTIME_METHOD := "runtime.script.run"
## The longest source accepted, and the longest return-value spelling an
## answer carries, in characters.
const MAX_SOURCE := 262144
const MAX_LITERAL := 262144
## The most log entries one answer carries.
const MAX_LOG_ENTRIES := 200
const TOOL_ANNOTATION := "(?m)^[ \\t]*@tool(?![A-Za-z0-9_])"


static func fail(kind: String, message: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": "not_started"}


## The source as a tool script: unchanged when it carries `@tool` itself, and
## otherwise with `@tool ` in front of its first line.
static func as_tool(source: String) -> String:
	var annotation := RegEx.new()
	annotation.compile(TOOL_ANNOTATION)
	if annotation.search(source) != null:
		return source
	return "@tool " + source


## Run `payload`'s source for `method`. `offered` says whether the session's
## server listed the method; `editor_log` is the log capture the report reads,
## or null; a source extending Node is added under `parent` when one is given.
func run(method: String, offered: bool, payload: Dictionary, editor_log, parent: Node) -> Dictionary:
	if not offered:
		return fail("permission_denied", "this session's server did not offer %s: its operator has not turned script execution on" % method)
	var source = payload.get("source")
	if typeof(source) != TYPE_STRING or source.is_empty() or source.length() > MAX_SOURCE:
		return fail("invalid_input", "`source` is a GDScript of 1 to %d characters" % MAX_SOURCE)
	var capturing: bool = editor_log != null and editor_log.capturing()
	var mark: int = editor_log.last() if capturing else 0
	var script := GDScript.new()
	script.source_code = as_tool(source)
	var own := "gdscript://%d.gd" % script.get_instance_id()
	var error := script.reload()
	if error != OK:
		return _answer({"stage": "compile", "ran": false, "reason": "the source does not compile (%s)" % error_string(error)}, own, editor_log, capturing, mark, method)
	if not script.is_tool():
		return _answer({"stage": "compile", "ran": true, "reason": "the source compiled but is not a tool script, so the editor cannot run it"}, own, editor_log, capturing, mark, method)
	if not script.can_instantiate():
		return _answer({"stage": "compile", "ran": true, "reason": "the source compiled, running its static initializers, but cannot be instantiated"}, own, editor_log, capturing, mark, method)
	var instance = script.new()
	if not instance.has_method("run"):
		_release(instance)
		return _answer({"stage": "compile", "ran": true, "reason": "the source compiled, running its static initializers, but defines no run() method"}, own, editor_log, capturing, mark, method)
	if instance is Node and parent != null:
		parent.add_child(instance)
	var value = await instance.run()
	var literal := var_to_str(value)
	_release(instance)
	var report := {"stage": "done", "ran": true}
	if literal.length() > MAX_LITERAL:
		report["literal"] = null
		report["literal_length"] = literal.length()
	else:
		report["literal"] = literal
	return _answer(report, own, editor_log, capturing, mark, method)


func _answer(report: Dictionary, own: String, editor_log, capturing: bool, mark: int, method: String) -> Dictionary:
	report["source_file"] = own
	report["log_capture"] = capturing
	report["action"] = "godot-mcp: %s" % method
	if capturing:
		var read: Dictionary = editor_log.read({"after": mark, "limit": MAX_LOG_ENTRIES})
		report["log"] = read.get("entries", [])
		report["log_skipped"] = int(read.get("skipped", 0))
		report["log_truncated"] = bool(read.get("truncated", false)) or int(read.get("next", 0)) < editor_log.last()
	else:
		report["log"] = []
		report["log_skipped"] = 0
		report["log_truncated"] = false
	return {"ok": true, "payload": report, "mutation_status": "committed"}


func _release(instance) -> void:
	if not is_instance_valid(instance):
		return
	if instance is Node:
		if instance.get_parent() != null:
			instance.get_parent().remove_child(instance)
		instance.queue_free()
	elif not (instance is RefCounted):
		instance.free()
