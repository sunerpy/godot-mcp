@tool
extends RefCounted
## `editor.preferences.read` and `editor.preferences.set`: the editor's own
## settings, the ones its Editor Settings dialog shows.
##
## An editor setting is not project content: the editor keeps it in the user's
## settings file, which every project shares. A setting that holds a path, a
## host, a port or a credential is listed with its type, but its value and hint
## are withheld. None of those can be changed from here, nor one that chooses a
## program the editor starts or makes it rescan and import on its own.
##
## A change is all or nothing. Every value is checked first: it must be of the
## setting's type and inside its range, choices or flags. The editor then takes
## the values, is told its settings changed, as its Editor Settings dialog
## tells it, and saves them. Godot 4.5's dialog does neither while it is
## hidden, so this does both itself. The editor may change more settings in
## response, or put a value back, as `editor_config.gd` describes: a change
## counts once every value named is kept and the saved file holds everything
## that changed; a value at its default may be left out of it. When a value is
## not kept or the file cannot be written, everything is put back.

const VariantCodec = preload("variant_codec.gd")
const EditorConfig = preload("editor_config.gd")

# The protected editor settings, as crates/godot-mcp-domain/src/editor_preferences.rs
# lists them; a test holds this copy to that table. Change the table, not this copy.
const WITHHELD_PREFIXES := ["export/", "filesystem/file_server/", "network/", "version_control/"]
const WITHHELD_HINTS := [PROPERTY_HINT_FILE, PROPERTY_HINT_DIR, PROPERTY_HINT_GLOBAL_FILE, PROPERTY_HINT_GLOBAL_DIR, PROPERTY_HINT_PASSWORD]
const FIXED_PREFIXES := ["asset_library/", "asset_store/", "filesystem/external_programs/", "filesystem/import/", "filesystem/tools/", "text_editor/external/"]
const FIXED_SETTINGS := ["docks/filesystem/other_file_extensions", "docks/filesystem/textfile_extensions", "interface/editor/behavior/import_resources_when_unfocused", "interface/editor/import_resources_when_unfocused"]
const MAX_CHANGES := 64

const MAX_PAGE := 500
## What follows the refusal when the editor puts back a setting a preset sets.
const PRESET_HINT := "A setting that a theme preset or color theme sets keeps a value of its own only while that preset is Custom: set the preset, such as interface/theme/color_preset (interface/theme/preset before Godot 4.6) or text_editor/theme/color_theme, to Custom in the same change, as the Editor Settings dialog does"
## The property hints an editor setting uses, by the name an answer gives them.
const HINT_NAMES := {
	PROPERTY_HINT_NONE: "none",
	PROPERTY_HINT_RANGE: "range",
	PROPERTY_HINT_ENUM: "enum",
	PROPERTY_HINT_ENUM_SUGGESTION: "enum_suggestion",
	PROPERTY_HINT_EXP_EASING: "exp_easing",
	PROPERTY_HINT_LINK: "link",
	PROPERTY_HINT_FLAGS: "flags",
	PROPERTY_HINT_FILE: "file",
	PROPERTY_HINT_DIR: "dir",
	PROPERTY_HINT_GLOBAL_FILE: "global_file",
	PROPERTY_HINT_GLOBAL_DIR: "global_dir",
	PROPERTY_HINT_RESOURCE_TYPE: "resource_type",
	PROPERTY_HINT_MULTILINE_TEXT: "multiline_text",
	PROPERTY_HINT_EXPRESSION: "expression",
	PROPERTY_HINT_PLACEHOLDER_TEXT: "placeholder_text",
	PROPERTY_HINT_COLOR_NO_ALPHA: "color_no_alpha",
	PROPERTY_HINT_LOCALE_ID: "locale_id",
	PROPERTY_HINT_PASSWORD: "password",
}


static func _ok(payload: Dictionary, status: String) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": status}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func _settings() -> EditorSettings:
	return EditorConfig.settings()


## Whether the setting holds a path, a host, a port or a credential, whose
## value and hint are not read.
static func _withheld(name: String, property: Dictionary) -> bool:
	if WITHHELD_HINTS.has(int(property["hint"])):
		return true
	for prefix in WITHHELD_PREFIXES:
		if name.begins_with(prefix):
			return true
	return false


## Why the setting may not be changed from here, or "".
static func _refusal(name: String, property: Dictionary) -> String:
	if _withheld(name, property):
		return "it holds a path, a host, a port or a credential"
	for prefix in FIXED_PREFIXES:
		if name.begins_with(prefix):
			return "it chooses a program the editor starts or where it downloads from"
	if FIXED_SETTINGS.has(name):
		return "changing it makes the editor rescan and import files on its own"
	return ""


static func _hint_name(hint: int) -> String:
	return HINT_NAMES.get(hint, "hint %d" % hint)


## The contract's tag for values of the type `value` has.
static func _tag(value) -> String:
	var encoded: Dictionary = VariantCodec.encode(type_convert(null, typeof(value)))
	return str(encoded["value"]["type"]) if encoded.get("ok", false) else type_string(typeof(value))


static func _restart(property: Dictionary) -> bool:
	return (int(property["usage"]) & PROPERTY_USAGE_RESTART_IF_CHANGED) != 0


static func _entry(settings: EditorSettings, name: String, property: Dictionary) -> Dictionary:
	var value = settings.get_setting(name)
	var refusal := _refusal(name, property)
	var entry := {
		"name": name,
		"type": _tag(value),
		"hint": _hint_name(int(property["hint"])),
		"restart_required": _restart(property),
		"changeable": refusal == "",
	}
	if refusal != "":
		entry["refusal"] = refusal
	if _withheld(name, property):
		entry["withheld"] = true
		return entry
	entry["hint_string"] = str(property["hint_string"])
	var encoded: Dictionary = VariantCodec.encode(value)
	if not encoded.get("ok", false):
		entry["withheld"] = true
		entry["unreadable"] = str(encoded.get("reason", ""))
		return entry
	entry["value"] = encoded["value"]
	if settings.property_can_revert(name):
		var default = settings.property_get_revert(name)
		var encoded_default: Dictionary = VariantCodec.encode(default)
		entry["default"] = encoded_default["value"] if encoded_default.get("ok", false) else null
		entry["changed"] = not EditorConfig.same(value, default)
	else:
		entry["default"] = null
		entry["changed"] = null
	return entry


## `editor.preferences.read`: the named settings, or every one in name order,
## under `prefix`, `limit` of them from `offset`. A named setting the editor
## does not have is listed in `unknown`.
static func read(payload: Dictionary) -> Dictionary:
	var settings := _settings()
	var listed := EditorConfig.listed(settings)
	var names: Array = []
	var unknown: Array = []
	var named = payload.get("settings")
	if typeof(named) == TYPE_ARRAY:
		for entry in named:
			var name := str(entry)
			if names.has(name) or unknown.has(name):
				continue
			if listed.has(name):
				names.append(name)
			else:
				unknown.append(name)
	else:
		names = listed.keys()
		names.sort()
	var prefix := str(payload.get("prefix", ""))
	if prefix != "":
		names = names.filter(func(name): return name.begins_with(prefix))
	var offset := maxi(int(payload.get("offset", 0)), 0)
	var limit := clampi(int(payload.get("limit", 100)), 1, MAX_PAGE)
	var page: Array = names.slice(offset, offset + limit)
	var entries: Array = []
	for name in page:
		entries.append(_entry(settings, name, listed[name]))
	var end := offset + page.size()
	return _ok({
		"settings": entries,
		"unknown": unknown,
		"total": names.size(),
		"offset": offset,
		"next_offset": end if end < names.size() else null,
	}, "not_started")


## Whether a tagged value names a resource or an object at any depth.
static func _refers(tagged) -> bool:
	if typeof(tagged) != TYPE_DICTIONARY:
		return false
	var kind := str(tagged.get("type", ""))
	if kind == "resource" or kind == "object":
		return true
	var value = tagged.get("value")
	if kind == "array" and typeof(value) == TYPE_ARRAY:
		return value.any(func(item): return _refers(item))
	if kind == "dictionary" and typeof(value) == TYPE_DICTIONARY:
		return value.values().any(func(item): return _refers(item))
	return false


## The values an enum hint allows: for an integer setting each option's
## explicit `:value`, or one more than the option before; for a text setting
## each option's text, or of a `value/label` option the part before the slash.
static func _choices(hint_string: String, text: bool) -> Array:
	var choices: Array = []
	var separator := ";" if hint_string.contains(";") else ","
	var current := 0
	for option in hint_string.split(separator, false):
		if text:
			choices.append(option.get_slice("/", 0) if separator == ";" else option)
			continue
		var at := option.rfind(":")
		if at != -1:
			current = int(option.substr(at + 1))
		choices.append(current)
		current += 1
	return choices


## Why `value` is not one the setting's hint allows, or "".
static func _outside(value, property: Dictionary) -> String:
	var hint := int(property["hint"])
	var hint_string := str(property["hint_string"])
	var number := typeof(value) == TYPE_INT or typeof(value) == TYPE_FLOAT
	if hint == PROPERTY_HINT_RANGE and number:
		var parts := hint_string.split(",")
		if parts.size() >= 2:
			if not parts.has("or_less") and value < float(parts[0]):
				return "%s is below its least value, %s" % [value, parts[0]]
			if not parts.has("or_greater") and value > float(parts[1]):
				return "%s is above its greatest value, %s" % [value, parts[1]]
	elif hint == PROPERTY_HINT_ENUM and (typeof(value) == TYPE_INT or typeof(value) == TYPE_STRING):
		var choices := _choices(hint_string, typeof(value) == TYPE_STRING)
		if not choices.has(value):
			return "%s is none of its choices (%s)" % [value, ", ".join(choices.map(func(choice): return str(choice)))]
	elif hint == PROPERTY_HINT_FLAGS and typeof(value) == TYPE_INT:
		var mask := 0
		var bit := 1
		for option in hint_string.split(",", false):
			var at := option.rfind(":")
			mask |= int(option.substr(at + 1)) if at != -1 else bit
			bit <<= 1
		if value < 0 or (value & ~mask) != 0:
			return "%d sets a flag it does not have (its flags make %d)" % [value, mask]
	return ""


## What a change made beside the ones named: each setting the editor changed
## in response, with its values before and after unless it holds a path, a
## host, a port or a credential, and each shortcut and built-in action, with
## its key events before and after.
static func also_changed(settings: EditorSettings, before: Dictionary, outcome: Dictionary, wanted: Dictionary) -> Dictionary:
	var listed := EditorConfig.listed(settings)
	var values: Array = []
	for name in outcome["changed"]["settings"]:
		if wanted.get("settings", {}).has(name):
			continue
		var entry := {"name": name}
		if listed.has(name) and _withheld(name, listed[name]):
			entry["withheld"] = true
		else:
			entry["before"] = _encoded(before["settings"].get(name))
			entry["after"] = _encoded(outcome["now"]["settings"].get(name))
		values.append(entry)
	var keys: Array = []
	for part in ["shortcuts", "actions"]:
		for name in outcome["changed"][part]:
			if wanted.get(part, {}).has(name):
				continue
			keys.append({
				"name": name,
				"kind": "shortcut" if part == "shortcuts" else "action",
				"before": before[part].get(name, []).map(func(event): return EditorConfig.event_json(event)),
				"after": outcome["now"][part].get(name, []).map(func(event): return EditorConfig.event_json(event)),
			})
	return {"settings": values, "shortcuts": keys}


static func _encoded(value):
	var encoded: Dictionary = VariantCodec.encode(value)
	return encoded["value"] if encoded.get("ok", false) else null


## `editor.preferences.set`: change every setting `settings` names to its
## tagged value, or none of them.
static func change(payload: Dictionary) -> Dictionary:
	var changes = payload.get("settings")
	if typeof(changes) != TYPE_DICTIONARY or changes.is_empty() or changes.size() > MAX_CHANGES:
		return _fail("invalid_input", "`settings` maps 1 to %d setting names to tagged values" % MAX_CHANGES)
	var settings := _settings()
	var listed := EditorConfig.listed(settings)
	var names: Array = changes.keys()
	names.sort()
	var plan: Array = []
	for name in names:
		if not listed.has(name):
			return _fail("not_found", "%s is not one of this editor's settings, so nothing was changed; editor.preferences.get lists them" % name)
		var property: Dictionary = listed[name]
		var refusal := _refusal(name, property)
		if refusal != "":
			return _fail("permission_denied", "%s cannot be changed from here: %s. Nothing was changed" % [name, refusal])
		if _refers(changes[name]):
			return _fail("invalid_input", "%s: an editor setting holds a plain value, never a resource" % name)
		var decoded: Dictionary = VariantCodec.decode(changes[name])
		if not decoded.get("ok", false):
			return _fail("invalid_input", "%s: %s" % [name, decoded.get("reason", "not a tagged value")])
		var value = decoded["value"]
		var current = settings.get_setting(name)
		if typeof(value) != typeof(current) and typeof(value) != int(property["type"]):
			return _fail("invalid_input", "%s holds a value of type %s, not %s. Nothing was changed" % [name, _tag(current), _tag(value)])
		var outside := _outside(value, property)
		if outside != "":
			return _fail("invalid_input", "%s: %s. Nothing was changed" % [name, outside])
		plan.append({"name": name, "before": current, "after": value, "restart": _restart(property)})
	var editor_node := EditorConfig.editor_node()
	if editor_node == null:
		return _fail("unavailable", "the editor's own node was not found, so its settings could not be applied; nothing was changed")

	var before := EditorConfig.capture(settings)
	var file_before := EditorConfig.file_sha(settings.resource_path)
	var wanted := {"settings": {}}
	for step in plan:
		settings.set_setting(step["name"], step["after"])
		wanted["settings"][step["name"]] = step["after"]
	var outcome: Dictionary = await EditorConfig.commit(settings, editor_node, before, file_before, wanted, PRESET_HINT)
	if not outcome["ok"]:
		return outcome
	var changed: Array = []
	for step in plan:
		changed.append({
			"name": step["name"],
			"before": VariantCodec.encode(step["before"])["value"],
			"after": VariantCodec.encode(step["after"])["value"],
			"restart_required": step["restart"],
		})
	return _ok({
		"settings": changed,
		"also_changed": also_changed(settings, before, outcome, wanted),
		"saved": true,
		"restart_required": plan.any(func(step): return step["restart"]),
		"action": "godot-mcp: editor.preferences.set",
		"transaction_id": str(payload.get("transaction_id", "")),
	}, "committed")
