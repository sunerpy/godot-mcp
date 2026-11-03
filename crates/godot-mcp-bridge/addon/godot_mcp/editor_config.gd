@tool
extends RefCounted
## The editor's own configuration as one state: every setting its Editor
## Settings dialog shows and, on Godot 4.6 and later, every shortcut and
## built-in action its Shortcuts tab lists. It is not project content: the
## editor keeps it in the user's settings file, which every project shares.
##
## `editor.preferences.set` and `editor.shortcuts.set` each change part of it,
## and the editor may change more in response, or put a value back: Godot
## 4.7's dialog switches a theme preset to Custom when one of its colors
## changes and applies a 3D navigation scheme's mouse buttons and modifier
## keys when the scheme changes, while earlier releases put such a color back
## to the preset's. So a change is held to the whole state. It is captured
## before; the editor is told of the change in a frame of its own, where it
## reconsiders every setting changed so far, as it does once a frame; every
## part named must then hold what was asked, or everything is put back; and
## the saved file must hold every part that changed.

## The modifiers of a key event, by what an answer calls them.
const KEY_FLAGS := ["shift", "ctrl", "alt", "meta", "command_or_control"]
## How a key event matches its key: by the key the layout maps, by its
## position, or by the character the layout prints on it.
const KEY_MATCHES := ["keycode", "physical", "label"]
## Rounds of putting the configuration back: putting one setting back can make
## the editor change another again, as it does when it changes them first.
const RESTORE_ROUNDS := 3


static func settings() -> EditorSettings:
	return EditorInterface.get_editor_settings()


## Whether this editor names its shortcuts to a plugin: false on a Godot
## before 4.6, whose EditorSettings has no get_shortcut_list.
static func has_shortcuts() -> bool:
	return settings().has_method("get_shortcut_list")


## Every setting the Editor Settings dialog shows, by name, with its property.
static func listed(editor_settings: EditorSettings) -> Dictionary:
	var found := {}
	for property in editor_settings.get_property_list():
		var name: String = property["name"]
		if (int(property["usage"]) & PROPERTY_USAGE_EDITOR) != 0 and name.contains("/"):
			found[name] = property
	return found


static func same(left, right) -> bool:
	return typeof(left) == typeof(right) and var_to_str(left) == var_to_str(right)


## A copy of a setting's value that a later change to the setting cannot
## reach.
static func _copy(value):
	if value is Array or value is Dictionary:
		return value.duplicate(true)
	if value is Object:
		return value
	if typeof(value) >= TYPE_PACKED_BYTE_ARRAY:
		return value.duplicate()
	return value


## The node the editor tells of changed settings, as EditorSettings does: the
## EditorNode, the first child of the editor's root window.
static func editor_node() -> Node:
	for child in EditorInterface.get_base_control().get_tree().root.get_children():
		if child.get_class() == "EditorNode":
			return child
	return null


static func file_sha(path: String) -> String:
	return FileAccess.get_sha256(path) if FileAccess.file_exists(path) else ""


# --- shortcuts and built-in actions ------------------------------------------


## Every editor shortcut the Shortcuts tab lists, by name: the ones the editor
## registered, which carry their default events as `original`. Empty before
## Godot 4.6.
static func shortcuts(editor_settings: EditorSettings) -> Dictionary:
	var found := {}
	if not editor_settings.has_method("get_shortcut_list"):
		return found
	for name in editor_settings.call("get_shortcut_list"):
		var shortcut = editor_settings.call("get_shortcut", name)
		if shortcut is Shortcut and shortcut.has_meta("original"):
			found[str(name)] = shortcut
	return found


static func _keys(events: Array) -> Array:
	return events.filter(func(event): return event is InputEventKey)


## The editor's built-in input actions, those of the Shortcuts tab's Common
## section, by name, each with its default key events. The engine's defaults
## are the defaults of the project's `input/` settings: an action's own, or
## its override for a feature this platform has, such as `.macos`. The editor
## keeps only their key events, as it does on start.
static func actions() -> Dictionary:
	var defaults := {}
	var overridden := {}
	for property in ProjectSettings.get_property_list():
		var full: String = property["name"]
		if not full.begins_with("input/"):
			continue
		var initial = ProjectSettings.property_get_revert(full)
		if typeof(initial) != TYPE_DICTIONARY or typeof(initial.get("events")) != TYPE_ARRAY:
			continue
		var action := full.substr(6)
		var feature := ""
		if action.contains("."):
			feature = action.get_slice(".", 1)
			action = action.get_slice(".", 0)
			if not OS.has_feature(feature):
				continue
		if not InputMap.has_action(action) or (feature == "" and overridden.has(action)):
			continue
		defaults[action] = _keys(initial["events"])
		if feature != "":
			overridden[action] = true
	return defaults


## A key event as the shortcut tools answer it: its key, how it is matched,
## its modifiers, and the editor's own text for it. `command_or_control` is
## Ctrl, or Command on macOS, and leaves `ctrl` and `meta` false. The editor
## matches an event by its keycode when it has one, else by its physical
## keycode, else by its label.
static func event_json(event) -> Dictionary:
	if not (event is InputEventKey):
		return {"class": event.get_class() if event != null else "null", "text": event.as_text() if event != null else ""}
	var how := "keycode"
	var code: int = event.keycode
	if code == KEY_NONE and event.physical_keycode != KEY_NONE:
		how = "physical"
		code = event.physical_keycode
	elif code == KEY_NONE and event.key_label != KEY_NONE:
		how = "label"
		code = event.key_label
	var remapped: bool = event.command_or_control_autoremap
	return {
		"key": OS.get_keycode_string(code),
		"match": how,
		"shift": event.shift_pressed,
		"ctrl": event.ctrl_pressed and not remapped,
		"alt": event.alt_pressed,
		"meta": event.meta_pressed and not remapped,
		"command_or_control": remapped,
		"text": event.as_text(),
	}


## The key event `json` describes, as {"event": InputEventKey}, or why it
## describes none, as {"reason": String}.
static func event_from(json) -> Dictionary:
	if typeof(json) != TYPE_DICTIONARY or typeof(json.get("key")) != TYPE_STRING:
		return {"reason": "every key event is an object that names its `key`"}
	var name: String = json["key"]
	for field in json:
		if field == "key":
			continue
		if field == "text":
			# The editor's own text an answer adds, given back with the event; not read.
			if typeof(json[field]) != TYPE_STRING:
				return {"reason": "a key event gives `text`, when it does, as a string"}
			continue
		if field == "match":
			if typeof(json[field]) != TYPE_STRING or not KEY_MATCHES.has(json[field]):
				return {"reason": "a key event matches its key by one of %s" % ", ".join(KEY_MATCHES)}
			continue
		if not KEY_FLAGS.has(field) or typeof(json[field]) != TYPE_BOOL:
			return {"reason": "a key event holds only `key`, `match`, `text` and the flags %s, each a boolean" % ", ".join(KEY_FLAGS)}
	var code := OS.find_keycode_from_string(name)
	if name == "" or code == KEY_NONE or (code & KEY_MODIFIER_MASK) != 0:
		return {"reason": "`%s` names no key Godot knows; name one as editor.shortcuts.get answers keys, such as S, F9, Enter or Kp Add, and give its modifiers as flags" % name}
	var flag := func(field: String) -> bool: return json.get(field, false)
	if flag.call("command_or_control") and (flag.call("ctrl") or flag.call("meta")):
		return {"reason": "a key event with `command_or_control` leaves `ctrl` and `meta` to the platform"}
	var event := InputEventKey.new()
	var how: String = json.get("match", "keycode")
	if how == "physical":
		event.physical_keycode = code
	elif how == "label":
		event.key_label = code
	else:
		event.keycode = code
	if flag.call("command_or_control"):
		event.command_or_control_autoremap = true
	else:
		event.ctrl_pressed = flag.call("ctrl")
		event.meta_pressed = flag.call("meta")
	event.shift_pressed = flag.call("shift")
	event.alt_pressed = flag.call("alt")
	return {"event": event}


## A key event as the editor matches it, by its key and the modifiers that
## must be held: two events it cannot tell apart give the same text.
static func event_key(event) -> String:
	if not (event is InputEventKey):
		return "%s:%s" % [event.get_class(), event.as_text()] if event != null else "null"
	var mask := int(event.get_modifiers_mask())
	if event.keycode != KEY_NONE:
		return "k%d+%d" % [event.keycode, mask]
	if event.physical_keycode != KEY_NONE:
		return "p%d+%d" % [event.physical_keycode, mask]
	if event.key_label != KEY_NONE:
		return "l%d+%d" % [event.key_label, mask]
	return "none+%d" % mask


static func events_key(events: Array) -> String:
	return ",".join(events.map(func(event): return event_key(event)))


static func _copies(events: Array) -> Array:
	return events.map(func(event): return event.duplicate() if event is Resource else event)


# --- the whole state ---------------------------------------------------------


## The configuration as it is now: every listed setting's value, and every
## shortcut's and built-in action's events.
static func capture(editor_settings: EditorSettings) -> Dictionary:
	var values := {}
	for name in listed(editor_settings):
		values[name] = _copy(editor_settings.get_setting(name))
	var keys := {}
	var found := shortcuts(editor_settings)
	for name in found:
		keys[name] = _copies(found[name].events)
	var bound := {}
	if editor_settings.has_method("get_shortcut_list"):
		for name in actions():
			bound[name] = _copies(InputMap.action_get_events(name))
	return {"settings": values, "shortcuts": keys, "actions": bound}


## The settings, shortcuts and actions whose values or events differ between
## two captures, by name.
static func differences(before: Dictionary, after: Dictionary) -> Dictionary:
	var found := {}
	for part in ["settings", "shortcuts", "actions"]:
		var names := {}
		for name in before[part]:
			names[name] = true
		for name in after[part]:
			names[name] = true
		var differing: Array = []
		for name in names:
			if not before[part].has(name) or not after[part].has(name):
				differing.append(name)
			elif part == "settings":
				if not same(before[part][name], after[part][name]):
					differing.append(name)
			elif events_key(before[part][name]) != events_key(after[part][name]):
				differing.append(name)
		differing.sort()
		found[part] = differing
	return found


static func _none(found: Dictionary) -> bool:
	return found["settings"].is_empty() and found["shortcuts"].is_empty() and found["actions"].is_empty()


static func notify(node: Node) -> void:
	node.propagate_notification(EditorSettings.NOTIFICATION_EDITOR_SETTINGS_CHANGED)


## Put every part that differs from `before` back, telling the editor, until
## nothing differs or the rounds are spent. Returns whether nothing differs.
static func restore(editor_settings: EditorSettings, node: Node, before: Dictionary) -> bool:
	for _round in RESTORE_ROUNDS:
		var differing := differences(before, capture(editor_settings))
		if _none(differing):
			return true
		var found := shortcuts(editor_settings)
		for name in differing["settings"]:
			if before["settings"].has(name):
				editor_settings.set_setting(name, _copy(before["settings"][name]))
		for name in differing["shortcuts"]:
			if before["shortcuts"].has(name) and found.has(name):
				found[name].events = _copies(before["shortcuts"][name])
		for name in differing["actions"]:
			if before["actions"].has(name):
				editor_settings.call("set_builtin_action_override", name, _copies(before["actions"][name]))
		if not differing["shortcuts"].is_empty():
			editor_settings.mark_setting_changed("shortcuts")
		if not differing["actions"].is_empty():
			editor_settings.mark_setting_changed("builtin_action_overrides")
		notify(node)
	return _none(differences(before, capture(editor_settings)))


## The parts of `wanted` that `now` does not hold.
static func _not_kept(now: Dictionary, wanted: Dictionary) -> Array:
	var lost: Array = []
	for name in wanted.get("settings", {}):
		if not now["settings"].has(name) or not same(now["settings"][name], wanted["settings"][name]):
			lost.append(name)
	for part in ["shortcuts", "actions"]:
		for name in wanted.get(part, {}):
			if not now[part].has(name) or events_key(now[part][name]) != events_key(wanted[part][name]):
				lost.append(name)
	return lost


# --- the saved file ----------------------------------------------------------


## Each setting's value text in the editor's settings file, by name. A value
## may span lines; a new setting starts a line with `name = `.
static func written(text: String) -> Dictionary:
	var found := {}
	var name := ""
	var in_resource := false
	for line in text.split("\n"):
		if line.begins_with("["):
			in_resource = line == "[resource]"
			name = ""
			continue
		if not in_resource:
			continue
		var at := line.find(" = ")
		var key := line.substr(0, at) if at > 0 else ""
		if key != "" and not key.contains(" ") and not key.contains("\"") and not line.begins_with("\t"):
			name = key
			found[name] = line.substr(at + 3)
		elif name != "":
			found[name] += "\n" + line
	return found


## The key events the settings file stores as its sub-resources, by id, each
## built from the properties the file holds for it.
static func _stored_events(text: String) -> Dictionary:
	var found := {}
	var event: InputEventKey = null
	var properties := {}
	for line in text.split("\n") + PackedStringArray(["["]):
		if line.begins_with("["):
			if event != null:
				# Remapping clears Ctrl and Meta, so it is applied first.
				if properties.has("command_or_control_autoremap"):
					event.set("command_or_control_autoremap", properties["command_or_control_autoremap"])
				for property in properties:
					if property != "command_or_control_autoremap":
						event.set(property, properties[property])
			event = null
			properties = {}
			if line.begins_with("[sub_resource ") and line.contains("type=\"InputEventKey\""):
				var at := line.find("id=\"")
				if at != -1:
					var id := line.substr(at + 4, line.find("\"", at + 4) - at - 4)
					event = InputEventKey.new()
					found[id] = event
			continue
		if event != null:
			var equals := line.find(" = ")
			if equals > 0:
				properties[line.substr(0, equals)] = str_to_var(line.substr(equals + 3))
	return found


## The events the file's `shortcuts` or `builtin_action_overrides` list holds,
## by name, or null when the file holds that list in a form not understood.
static func _stored_list(text: String, written_values: Dictionary, list: String, field: String):
	if not written_values.has(list):
		return {}
	var events := _stored_events(text)
	var source: String = written_values[list]
	var reference := RegEx.create_from_string("SubResource\\(\"([^\"]+)\"\\)")
	var parsed = str_to_var(reference.sub(source, "\"@sub:$1\"", true))
	if typeof(parsed) != TYPE_ARRAY:
		return null
	var found := {}
	for entry in parsed:
		if typeof(entry) != TYPE_DICTIONARY or typeof(entry.get("name")) != TYPE_STRING or typeof(entry.get(field)) != TYPE_ARRAY:
			return null
		var resolved: Array = []
		for item in entry[field]:
			var id := str(item).trim_prefix("@sub:")
			if typeof(item) != TYPE_STRING or not str(item).begins_with("@sub:") or not events.has(id):
				return null
			resolved.append(events[id])
		found[entry["name"]] = resolved
	return found


## The parts of the configuration that changed, as `changed` names them, which
## the saved file does not hold as `now` has them. The editor writes a setting
## as `name = value` and a changed shortcut or action in its list of
## overrides, and may leave out any at its default.
static func not_in_file(editor_settings: EditorSettings, changed: Dictionary, now: Dictionary) -> Array:
	var text := FileAccess.get_file_as_string(editor_settings.resource_path)
	var values := written(text)
	var missing: Array = []
	for name in changed["settings"]:
		if not now["settings"].has(name):
			missing.append(name)
		elif values.has(name):
			if not same(str_to_var(str(values[name]).strip_edges()), now["settings"][name]):
				missing.append(name)
		elif not (editor_settings.property_can_revert(name) and same(editor_settings.property_get_revert(name), now["settings"][name])):
			missing.append(name)
	if changed["shortcuts"].is_empty() and changed["actions"].is_empty():
		return missing
	var stored_shortcuts = _stored_list(text, values, "shortcuts", "shortcuts")
	var stored_actions = _stored_list(text, values, "builtin_action_overrides", "events")
	var found := shortcuts(editor_settings)
	var defaults := actions()
	for name in changed["shortcuts"]:
		var default: Array = found[name].get_meta("original") if found.has(name) else []
		if not _stored_as(stored_shortcuts, name, now["shortcuts"].get(name), default):
			missing.append(name)
	for name in changed["actions"]:
		if not _stored_as(stored_actions, name, now["actions"].get(name), defaults.get(name, [])):
			missing.append(name)
	return missing


## Whether a stored list holds `events` for `name`, or leaves `name` out while
## `events` are its defaults.
static func _stored_as(stored, name: String, events, default: Array) -> bool:
	if stored == null or events == null:
		return false
	if stored.has(name):
		return events_key(stored[name]) == events_key(events)
	return events_key(events) == events_key(default)


# --- one change ----------------------------------------------------------------


static func _failure(kind: String, message: String, status: String) -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


## Hold the change just made, which `wanted` names (setting names to values,
## shortcut and action names to events), to the whole configuration, which
## `before` held, and save it; or put everything back. `hint` follows the
## refusal when the editor did not keep part of the change. Answers
## {"ok": true, "now": capture, "changed": differences} once the saved file
## holds every part that changed, or the failure to answer with.
static func commit(editor_settings: EditorSettings, node: Node, before: Dictionary, file_before: String, wanted: Dictionary, hint: String) -> Dictionary:
	# The editor decides once a frame whether its theme needs building again,
	# so it is told of the change in a frame of its own, where it reconsiders
	# every setting changed so far.
	await node.get_tree().process_frame
	notify(node)
	var now := capture(editor_settings)
	var lost := _not_kept(now, wanted)
	var why := ""
	var advice := ""
	if lost.is_empty():
		var saved := ResourceSaver.save(editor_settings)
		if saved == OK:
			var changed := differences(before, now)
			var missing := not_in_file(editor_settings, changed, now)
			if not missing.is_empty():
				return _failure("unavailable", "the editor saved its settings, but its settings file does not hold %s; whether the editor keeps them is not known" % ", ".join(missing), "unresolved")
			return {"ok": true, "now": now, "changed": changed}
		why = "the editor could not write its settings file (%s)" % error_string(saved)
	else:
		why = "the editor did not keep %s once it was told of the change" % ", ".join(lost)
		advice = (". " + hint) if hint != "" else ""
	# Nothing was saved: put everything back, and tell the editor so.
	var back := restore(editor_settings, node, before) and file_sha(editor_settings.resource_path) == file_before
	var kind := "unavailable" if lost.is_empty() else "conflict"
	if back:
		return _failure(kind, "%s, so everything was put back and nothing was changed%s" % [why, advice], "not_started")
	return _failure("unavailable", "%s, and putting everything back could not be proven; whether anything changed is not known" % why, "unresolved")
