@tool
extends RefCounted
## `editor.shortcuts.read` and `editor.shortcuts.set`: the editor's shortcuts,
## the ones its Editor Settings dialog lists on its Shortcuts tab, which
## Godot 4.6 and later name to a plugin.
##
## Two kinds are listed by name. A `shortcut` is one the editor registered
## (`script_editor/save`), which carries its default key events; an `action`
## is a built-in input action of the tab's Common section (`ui_accept`), whose
## defaults are the engine's own. A shortcut is not project content: the
## editor keeps a changed one in the user's settings file, which every project
## shares.
##
## A change is all or nothing. Every name and key event is checked first. The
## editor then takes the events, as its Shortcuts tab gives them, is told its
## settings changed and saves them; the change counts once the saved file
## holds every shortcut and action that changed, and anything the editor
## changed with them, as `editor_config.gd` describes. Nothing is added to an
## undo history.

const EditorConfig = preload("editor_config.gd")
const Preferences = preload("preferences.gd")

# The bounds of the shortcut tools, as crates/godot-mcp-domain/src/editor_shortcuts.rs
# states them; a test holds this copy to it. Change the module, not this copy.
const MAX_CHANGES := 64
const MAX_EVENTS := 16
const MAX_PAGE := 500
const DEFAULT_PAGE := 100


static func _ok(payload: Dictionary, status: String) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": status}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func _unsupported() -> Dictionary:
	return _fail("unsupported", "this editor cannot hand its shortcuts to a plugin: that needs EditorSettings.get_shortcut_list of Godot 4.6 or later. Nothing was read or changed")


## Every shortcut and built-in action, by name: its kind, label, current and
## default key events.
static func _entries(settings: EditorSettings) -> Dictionary:
	var found := {}
	var registered := EditorConfig.shortcuts(settings)
	for name in registered:
		var shortcut: Shortcut = registered[name]
		found[name] = {
			"kind": "shortcut",
			"label": shortcut.resource_name,
			"events": shortcut.events,
			"default": shortcut.get_meta("original"),
		}
	var actions := EditorConfig.actions()
	for name in actions:
		if not found.has(name):
			found[name] = {
				"kind": "action",
				"label": name,
				"events": InputMap.action_get_events(name),
				"default": actions[name],
			}
	return found


static func _changed(entry: Dictionary) -> bool:
	return EditorConfig.events_key(entry["events"]) != EditorConfig.events_key(entry["default"])


static func _describe(name: String, entry: Dictionary) -> Dictionary:
	return {
		"name": name,
		"kind": entry["kind"],
		"label": entry["label"],
		"events": entry["events"].map(func(event): return EditorConfig.event_json(event)),
		"default": entry["default"].map(func(event): return EditorConfig.event_json(event)),
		"changed": _changed(entry),
	}


## `editor.shortcuts.read`: the named shortcuts, or every one in name order,
## under `prefix`, only the changed ones with `changed_only`, `limit` of them
## from `offset`. A named shortcut the editor does not have is listed in
## `unknown`.
static func read(payload: Dictionary) -> Dictionary:
	if not EditorConfig.has_shortcuts():
		return _unsupported()
	var entries := _entries(EditorConfig.settings())
	var names: Array = []
	var unknown: Array = []
	var named = payload.get("shortcuts")
	if typeof(named) == TYPE_ARRAY:
		for item in named:
			var name := str(item)
			if names.has(name) or unknown.has(name):
				continue
			if entries.has(name):
				names.append(name)
			else:
				unknown.append(name)
	else:
		names = entries.keys()
		names.sort()
	var prefix := str(payload.get("prefix", ""))
	if prefix != "":
		names = names.filter(func(name): return name.begins_with(prefix))
	if payload.get("changed_only", false) == true:
		names = names.filter(func(name): return _changed(entries[name]))
	var offset := maxi(int(payload.get("offset", 0)), 0)
	var limit := clampi(int(payload.get("limit", DEFAULT_PAGE)), 1, MAX_PAGE)
	var page: Array = names.slice(offset, offset + limit)
	var end := offset + page.size()
	return _ok({
		"shortcuts": page.map(func(name): return _describe(name, entries[name])),
		"unknown": unknown,
		"total": names.size(),
		"offset": offset,
		"next_offset": end if end < names.size() else null,
	}, "not_started")


## `editor.shortcuts.set`: give every shortcut and action `shortcuts` names
## its key events, an empty list for none or `default` for its defaults, or
## change none of them.
static func change(payload: Dictionary) -> Dictionary:
	if not EditorConfig.has_shortcuts():
		return _unsupported()
	var changes = payload.get("shortcuts")
	if typeof(changes) != TYPE_DICTIONARY or changes.is_empty() or changes.size() > MAX_CHANGES:
		return _fail("invalid_input", "`shortcuts` maps 1 to %d shortcut names to key events or `default`" % MAX_CHANGES)
	var settings := EditorConfig.settings()
	var entries := _entries(settings)
	var registered := EditorConfig.shortcuts(settings)
	var names: Array = changes.keys()
	names.sort()
	var plan: Array = []
	for name in names:
		if not entries.has(name):
			return _fail("not_found", "%s is not one of this editor's shortcuts or built-in actions, so nothing was changed; editor.shortcuts.get lists them" % name)
		var entry: Dictionary = entries[name]
		var given = changes[name]
		var events: Array = []
		if typeof(given) == TYPE_STRING and given == "default":
			events = entry["default"].map(func(event): return event.duplicate())
		elif typeof(given) == TYPE_ARRAY and given.size() <= MAX_EVENTS:
			for item in given:
				var decoded := EditorConfig.event_from(item)
				if decoded.has("reason"):
					return _fail("invalid_input", "%s: %s. Nothing was changed" % [name, decoded["reason"]])
				events.append(decoded["event"])
		else:
			return _fail("invalid_input", "%s: a shortcut is given at most %d key events, or `default` for its default events. Nothing was changed" % [name, MAX_EVENTS])
		plan.append({"name": name, "kind": entry["kind"], "before": entry["events"].duplicate(), "after": events})
	var editor_node := EditorConfig.editor_node()
	if editor_node == null:
		return _fail("unavailable", "the editor's own node was not found, so its shortcuts could not be applied; nothing was changed")

	var before := EditorConfig.capture(settings)
	var file_before := EditorConfig.file_sha(settings.resource_path)
	var wanted := {"shortcuts": {}, "actions": {}}
	for step in plan:
		if step["kind"] == "shortcut":
			registered[step["name"]].events = step["after"]
			wanted["shortcuts"][step["name"]] = step["after"]
		else:
			settings.call("set_builtin_action_override", step["name"], step["after"])
			wanted["actions"][step["name"]] = step["after"]
	# As the Shortcuts tab does, so the editor's own views of them refresh.
	if not wanted["shortcuts"].is_empty():
		settings.mark_setting_changed("shortcuts")
	if not wanted["actions"].is_empty():
		settings.mark_setting_changed("builtin_action_overrides")
	var outcome: Dictionary = await EditorConfig.commit(settings, editor_node, before, file_before, wanted, "")
	if not outcome["ok"]:
		return outcome
	var changed: Array = []
	for step in plan:
		var part := "shortcuts" if step["kind"] == "shortcut" else "actions"
		changed.append({
			"name": step["name"],
			"kind": step["kind"],
			"before": step["before"].map(func(event): return EditorConfig.event_json(event)),
			"after": outcome["now"][part][step["name"]].map(func(event): return EditorConfig.event_json(event)),
		})
	return _ok({
		"shortcuts": changed,
		"also_changed": Preferences.also_changed(settings, before, outcome, wanted),
		"saved": true,
		"action": "godot-mcp: editor.shortcuts.set",
		"transaction_id": str(payload.get("transaction_id", "")),
	}, "committed")
