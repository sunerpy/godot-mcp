@tool
extends RefCounted
## `editor.performance.read`: the editor process's own Performance monitors,
## by the names this Godot build's Performance.Monitor enum gives them, and the
## custom monitors added in the editor process. The set of names differs
## between builds (39 on Godot 4.4.1, 59 on 4.5.1 to 4.7.1), so every value is
## answered under its name. A value that is not a finite number is answered as
## null.

const MAX_MONITORS := 256


## The id of every monitor of this build's Performance.Monitor enum, by name.
static func builtin() -> Dictionary:
	var named := {}
	for name in ClassDB.class_get_enum_constants("Performance", "Monitor"):
		if name != "MONITOR_MAX":
			named[String(name)] = ClassDB.class_get_integer_constant("Performance", name)
	return named


static func custom() -> Array:
	var names: Array = []
	for name in Performance.get_custom_monitor_names():
		names.append(String(name))
	return names


static func _finite(value):
	if (value is float or value is int) and is_finite(float(value)):
		return value
	return null


static func read(payload: Dictionary) -> Dictionary:
	var named := builtin()
	var custom_names := custom()
	var wanted = payload.get("monitors")
	if wanted != null:
		if typeof(wanted) != TYPE_ARRAY or wanted.size() > MAX_MONITORS:
			return {"ok": false, "error_kind": "invalid_input", "message": "`monitors` names at most %d monitors" % MAX_MONITORS, "mutation_status": "not_started"}
		for name in wanted:
			if not named.has(str(name)) and not custom_names.has(str(name)):
				var version: Dictionary = Engine.get_version_info()
				return {"ok": false, "error_kind": "invalid_input", "message": "%s is not a performance monitor of this editor (Godot %d.%d.%d) or of its custom monitors" % [str(name), int(version.get("major", 0)), int(version.get("minor", 0)), int(version.get("patch", 0))], "mutation_status": "not_started"}
	var monitors := {}
	for name in named:
		if wanted == null or wanted.has(name):
			monitors[name] = _finite(Performance.get_monitor(named[name]))
	var customs := {}
	for name in custom_names:
		if wanted == null or wanted.has(name):
			customs[name] = _finite(Performance.get_custom_monitor(StringName(name)))
	return {
		"ok": true,
		"payload": {"monitors": monitors, "custom": customs, "frame": Engine.get_process_frames()},
		"mutation_status": "not_started",
	}
