@tool
extends RefCounted
## `editor.commands.call`: call a command the project declares in
## `godot_mcp_commands.json` (ADR 0016). The server checked the call against
## the declaration before it sent it: the request names the provider, the
## command, the provider's script and the arguments, as tagged values, that the
## command's input schema accepts.
##
## The method is offered only by a server whose operator allows project
## commands: such a server lists it in the session's capabilities, and a
## request on a session whose capabilities did not list it is refused here
## before anything is loaded.
##
## The provider's script is read from its file again, not from a cached copy,
## and instantiated, and its `mcp_call(command, args)` is called, and awaited when
## it is a coroutine. Loading the script runs its static initializers. The code
## runs on the editor's main thread with the user's privileges, and nothing
## confines what it does. Godot cannot stop a script part way through: a
## command that never returns keeps the editor busy until the editor stops.
##
## The answer says how far the call got (`stage`): `compile` when the script
## did not load or cannot be instantiated, `provider` when it defines no
## `mcp_call`, `returned` when `mcp_call` returned anything but a Dictionary
## with a boolean `ok`, and `done` otherwise, with `ok`, the value spelled by
## Godot's own `var_to_str` or the error text, and what was logged meanwhile,
## on a Godot whose log a script can read (4.5 and later).

const VariantCodec = preload("variant_codec.gd")

const METHOD := "editor.commands.call"
## The longest provider id and command name, in characters.
const MAX_NAME := 64
## The longest script path, the longest value spelling and the longest error
## text an answer carries, in characters.
const MAX_SCRIPT_PATH := 1024
const MAX_LITERAL := 262144
const MAX_ERROR := 4096
## The most log entries one answer carries.
const MAX_LOG_ENTRIES := 200
const TOOL_ANNOTATION := "(?m)^[ \\t]*@tool(?![A-Za-z0-9_])"


static func fail(kind: String, message: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": "not_started"}


## Whether `path` is a project script path the server could have sent: a
## `res://` path to a `.gd` file with no empty, hidden, `.` or `..` segment.
static func script_path(path) -> bool:
	if typeof(path) != TYPE_STRING or path.length() > MAX_SCRIPT_PATH:
		return false
	if not path.begins_with("res://") or not path.ends_with(".gd") or path.contains("\\"):
		return false
	for segment in path.trim_prefix("res://").split("/"):
		if segment.is_empty() or segment.begins_with("."):
			return false
	return true


## Whether `source` carries GDScript's `@tool` annotation.
static func is_tool(source: String) -> bool:
	var annotation := RegEx.new()
	annotation.compile(TOOL_ANNOTATION)
	return annotation.search(source) != null


## Call `payload`'s command. `offered` says whether the session's server listed
## the method; `editor_log` is the log capture the report reads, or null; a
## provider extending Node is added under `parent` while it runs, when one is
## given.
func call_command(offered: bool, payload: Dictionary, editor_log, parent: Node) -> Dictionary:
	if not offered:
		return fail("permission_denied", "this session's server did not offer %s: its operator does not allow project commands" % METHOD)
	var provider = payload.get("provider")
	var command = payload.get("command")
	var path = payload.get("script")
	if typeof(provider) != TYPE_STRING or provider.is_empty() or provider.length() > MAX_NAME:
		return fail("invalid_input", "`provider` is a provider id of 1 to %d characters" % MAX_NAME)
	if typeof(command) != TYPE_STRING or command.is_empty() or command.length() > MAX_NAME:
		return fail("invalid_input", "`command` is a command name of 1 to %d characters" % MAX_NAME)
	if not script_path(path):
		return fail("invalid_input", "`script` is the res:// path of the provider's .gd script")
	var decoded: Dictionary = VariantCodec.decode(payload.get("arguments"))
	if not decoded["ok"]:
		return fail("invalid_input", "`arguments` are not a tagged value: %s" % decoded["reason"])
	var args = decoded["value"]
	if typeof(args) != TYPE_DICTIONARY:
		return fail("invalid_input", "`arguments` must be a tagged dictionary")
	if not FileAccess.file_exists(path):
		return fail("invalid_input", "the provider's script %s does not exist" % path)
	if not is_tool(FileAccess.get_file_as_string(path)):
		return fail("invalid_input", "the provider's script %s has no @tool line, so the editor cannot run it" % path)
	var capturing: bool = editor_log != null and editor_log.capturing()
	var mark: int = editor_log.last() if capturing else 0
	# Loading began: whatever happens from here may have run project code.
	var script = ResourceLoader.load(path, "", ResourceLoader.CACHE_MODE_IGNORE)
	if script == null or not (script is Script):
		return _answer({"stage": "compile", "ran": false, "reason": "the provider's script %s did not load" % path}, path, editor_log, capturing, mark)
	if not script.can_instantiate():
		return _answer({"stage": "compile", "ran": false, "reason": "the provider's script %s did not compile, or cannot be instantiated" % path}, path, editor_log, capturing, mark)
	var instance = script.new()
	if not instance.has_method("mcp_call"):
		_release(instance)
		return _answer({"stage": "provider", "ran": true, "reason": "the provider's script %s defines no mcp_call(command, args)" % path}, path, editor_log, capturing, mark)
	if instance is Node and parent != null:
		parent.add_child(instance)
	var answered = await instance.mcp_call(command, args)
	_release(instance)
	if typeof(answered) != TYPE_DICTIONARY or typeof(answered.get("ok")) != TYPE_BOOL:
		return _answer(_spelled({"stage": "returned", "ran": true}, answered), path, editor_log, capturing, mark)
	var report := {"stage": "done", "ran": true, "ok": answered["ok"]}
	if answered["ok"]:
		report = _spelled(report, answered.get("value"))
	else:
		var error = answered.get("error", "")
		var text := str(error) if typeof(error) == TYPE_STRING else var_to_str(error)
		report["error"] = text.substr(0, MAX_ERROR)
		report["literal"] = null
	return _answer(report, path, editor_log, capturing, mark)


## `report` with Godot's spelling of `value`, or its length when the spelling
## is longer than an answer carries.
static func _spelled(report: Dictionary, value) -> Dictionary:
	var literal := var_to_str(value)
	if literal.length() > MAX_LITERAL:
		report["literal"] = null
		report["literal_length"] = literal.length()
	else:
		report["literal"] = literal
	return report


func _answer(report: Dictionary, path: String, editor_log, capturing: bool, mark: int) -> Dictionary:
	report["source_file"] = path
	report["log_capture"] = capturing
	report["action"] = "godot-mcp: %s" % METHOD
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
