@tool
extends RefCounted
## `editor.settings.apply`: a typed project-settings change, carried out by the
## editor that owns project.godot while it runs.
##
## A running editor keeps every project setting in memory and writes them all
## back whenever it saves them, so a change written to the file underneath it
## is lost at its next save. The server therefore sends each changed setting as
## the literal project.godot holds before and after the change, and the editor
## applies it to its live settings and saves them with Godot's own writer.
##
## Nothing is changed unless every check passes first:
##
## * project.godot still hashes to what the server read;
## * each setting's live value is the one the file holds, so a value the user
##   changed but did not save is never overwritten;
## * each plugin switch starts from the state the file records;
## * a new value is a plain value or an InputEvent, never another object or a
##   resource to load.
##
## After saving, the file is read back and every setting compared; a mismatch
## is reported as committed with the settings that differ.

const PROJECT_FILE := "res://project.godot"
const OWN_PLUGIN := "res://addons/godot_mcp/plugin.cfg"
const MAX_CHANGES := 256
const MAX_PLUGINS := 64
const MAX_LITERAL := 65536


static func ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func apply(payload: Dictionary) -> Dictionary:
	var expected := str(payload.get("expected_sha256", ""))
	if FileAccess.get_sha256(PROJECT_FILE) != expected:
		return fail("conflict", "project.godot changed since the server read it; nothing was applied")
	var changes = payload.get("changes", [])
	var plugins = payload.get("plugins", [])
	if typeof(changes) != TYPE_ARRAY or typeof(plugins) != TYPE_ARRAY:
		return fail("invalid_input", "`changes` and `plugins` must be arrays")
	if changes.size() > MAX_CHANGES or plugins.size() > MAX_PLUGINS:
		return fail("invalid_input", "too many settings in one change")

	# Decide everything before anything is changed.
	var settings: Array = []
	for change in changes:
		if typeof(change) != TYPE_DICTIONARY:
			return fail("invalid_input", "every change must be an object")
		var key := str(change.get("key", ""))
		if not _is_setting_name(key):
			return fail("invalid_input", "`%s` is not a project setting name" % key)
		if not _holds(key, change.get("old")):
			return fail(
				"conflict",
				"the editor holds an unsaved value for %s; save or revert it in Project Settings, then retry. Nothing was applied" % key
			)
		var literal = change.get("new")
		if literal == null:
			settings.append({"key": key, "remove": true})
			continue
		var refused := _refused_literal(str(literal))
		if refused != "":
			return fail("invalid_input", "%s: %s" % [key, refused])
		settings.append({"key": key, "remove": false, "value": str_to_var(str(literal))})
	var switches: Array = []
	for entry in plugins:
		if typeof(entry) != TYPE_DICTIONARY:
			return fail("invalid_input", "every plugin switch must be an object")
		var path := str(entry.get("path", ""))
		var enabled := bool(entry.get("enabled", false))
		if not (path.begins_with("res://addons/") and path.ends_with("/plugin.cfg")):
			return fail("invalid_input", "`%s` is not a plugin's plugin.cfg" % path)
		if path == OWN_PLUGIN and not enabled:
			return fail("conflict", "disabling godot_mcp from the editor would cut the connection carrying this change; disable it in Project Settings → Plugins")
		if EditorInterface.is_plugin_enabled(path) == enabled:
			return fail(
				"conflict",
				"the editor already has %s %s, which project.godot does not record; save the project settings first. Nothing was applied" % [path, "enabled" if enabled else "disabled"]
			)
		switches.append({"path": path, "enabled": enabled})

	for setting in settings:
		var key: String = setting["key"]
		if not setting["remove"]:
			ProjectSettings.set_setting(key, setting["value"])
		elif ProjectSettings.property_can_revert(key):
			# A built-in setting goes back to its default, which is not saved.
			ProjectSettings.set_setting(key, ProjectSettings.property_get_revert(key))
		else:
			ProjectSettings.set_setting(key, null)
	for switch in switches:
		EditorInterface.set_plugin_enabled(switch["path"], switch["enabled"])
	var saved := ProjectSettings.save()
	if saved != OK:
		return fail(
			"unavailable",
			"ProjectSettings.save() failed (%s); whether project.godot was written is not known" % error_string(saved),
			"unresolved"
		)

	var mismatched := _mismatched(settings, switches)
	var sha256 := FileAccess.get_sha256(PROJECT_FILE)
	if not mismatched.is_empty():
		return fail(
			"conflict",
			"project.godot was saved, but these do not hold the requested values: %s" % ", ".join(mismatched),
			"committed"
		)
	return ok({
		"saved": true,
		"sha256": sha256,
		"digest": sha256,
		"action": "godot-mcp: project.settings.patch",
		"settings": settings.size(),
		"plugins": switches.size(),
	})


static func _is_setting_name(key: String) -> bool:
	return key.length() <= 256 and key.contains("/") and not key.begins_with("/") \
		and not key.contains("\n") and not key.contains("=") and not key.contains("[")


## Whether the editor's live value of `key` is the one the file holds: `old`
## is the file's literal, or null when the file has no such key and the live
## value must then be absent or the engine default.
static func _holds(key: String, old) -> bool:
	if old == null:
		if not ProjectSettings.has_setting(key):
			return true
		return ProjectSettings.property_can_revert(key) \
			and var_to_str(ProjectSettings.get_setting(key)) == var_to_str(ProjectSettings.property_get_revert(key))
	if not ProjectSettings.has_setting(key):
		return false
	return var_to_str(ProjectSettings.get_setting(key)) == var_to_str(str_to_var(str(old)))


## Why a new value's literal may not be applied, or "" when it may. A literal
## may construct InputEvent objects, which input actions hold, and nothing
## else: no other object and no resource to load.
static func _refused_literal(literal: String) -> String:
	if literal.length() > MAX_LITERAL:
		return "the value is too large"
	for token in ["Resource(", "ExtResource(", "SubResource("]:
		if literal.contains(token):
			return "a setting value may not load a resource"
	var at := literal.find("Object(")
	while at != -1:
		var rest := literal.substr(at + 7)
		var comma := rest.find(",")
		var named := rest.substr(0, comma if comma != -1 else rest.length()).strip_edges()
		if not named.begins_with("InputEvent") or not ClassDB.is_parent_class(named, "InputEvent"):
			return "a setting value may hold InputEvent objects only, not %s" % named
		at = literal.find("Object(", at + 7)
	return ""


## Every setting or plugin switch the saved file does not hold as requested.
static func _mismatched(settings: Array, switches: Array) -> Array:
	var file := ConfigFile.new()
	if file.load(PROJECT_FILE) != OK:
		return ["project.godot (it could not be read back)"]
	var differing: Array = []
	for setting in settings:
		var key: String = setting["key"]
		var section := key.get_slice("/", 0)
		var name := key.substr(section.length() + 1)
		var present := file.has_section_key(section, name)
		if setting["remove"]:
			if present:
				differing.append(key)
			continue
		var wanted := var_to_str(setting["value"])
		if present:
			if var_to_str(file.get_value(section, name)) != wanted:
				differing.append(key)
		elif not (ProjectSettings.property_can_revert(key) and var_to_str(ProjectSettings.property_get_revert(key)) == wanted):
			# Godot leaves a default out of the file; any other value must be there.
			differing.append(key)
	var enabled: PackedStringArray = file.get_value("editor_plugins", "enabled", PackedStringArray())
	for switch in switches:
		var path: String = switch["path"]
		if EditorInterface.is_plugin_enabled(path) != switch["enabled"] or enabled.has(path) != switch["enabled"]:
			differing.append(path)
	return differing
