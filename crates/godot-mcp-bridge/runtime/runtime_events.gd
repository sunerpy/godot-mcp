extends RefCounted
## Input events as the runtime probe records and replays them.
##
## A recorded event is a dictionary with its time `t_ms` from the start of the
## recording, its `kind` and that kind's fields: `key` (the key and physical key
## names it has — at least one — unicode, pressed, echo, modifiers),
## `mouse_button` (button, pressed, position, double_click), `mouse_motion`
## (position, relative, button_mask), `joypad_button` (device, button, pressed),
## `joypad_motion` (device, axis, value) and `action` (action, pressed,
## strength). Positions are window pixels, as the root window received them.
## `source` says whether the probe injected the event or something else
## delivered it, and `actions` names the input actions the event matched. Any
## other event, and a key event that names no key, is recorded as `other` with
## its class and is never replayed.
##
## A replayed event is built from the same dictionary; `source` and `actions`
## are what the recording observed and are not read.

## Mouse buttons by name, both directions.
const MOUSE_BUTTONS := {
	"left": MOUSE_BUTTON_LEFT,
	"right": MOUSE_BUTTON_RIGHT,
	"middle": MOUSE_BUTTON_MIDDLE,
	"wheel_up": MOUSE_BUTTON_WHEEL_UP,
	"wheel_down": MOUSE_BUTTON_WHEEL_DOWN,
	"wheel_left": MOUSE_BUTTON_WHEEL_LEFT,
	"wheel_right": MOUSE_BUTTON_WHEEL_RIGHT,
	"xbutton1": MOUSE_BUTTON_XBUTTON1,
	"xbutton2": MOUSE_BUTTON_XBUTTON2,
}
## The most action names one recorded event lists.
const MAX_ACTIONS := 8
## The highest joypad button and axis indices Godot defines.
const MAX_JOY_BUTTON := 127
const MAX_JOY_AXIS := 9
const MAX_DEVICE := 255
## The largest coordinate a replayed mouse event may carry.
const MAX_COORDINATE := 1000000.0


static func _xy(value: Vector2) -> Dictionary:
	return {"x": value.x, "y": value.y}


static func _modifiers(event: InputEventWithModifiers) -> Dictionary:
	return {
		"shift": event.shift_pressed,
		"ctrl": event.ctrl_pressed,
		"alt": event.alt_pressed,
		"meta": event.meta_pressed,
	}


static func _button_name(index: int):
	for name in MOUSE_BUTTONS:
		if MOUSE_BUTTONS[name] == index:
			return name
	return index


static func _key_name(code: int):
	if code == KEY_NONE:
		return null
	var name := OS.get_keycode_string(code)
	return name if name != "" else null


## The input actions `event` matches, at most MAX_ACTIONS of them.
static func matched_actions(event: InputEvent) -> Array:
	var matched: Array = []
	if event is InputEventAction:
		return matched
	for action in InputMap.get_actions():
		if event.is_action(action):
			matched.append(String(action))
			if matched.size() >= MAX_ACTIONS:
				break
	return matched


## `event`, received `t_ms` after the recording started, as a recorded
## dictionary; `injected` says whether the probe injected it.
static func describe(event: InputEvent, t_ms: int, injected: bool) -> Dictionary:
	var entry := {"t_ms": t_ms, "source": "probe" if injected else "external"}
	if event is InputEventKey:
		var key := event as InputEventKey
		var named = _key_name(key.keycode)
		var physical = _key_name(key.physical_keycode)
		if named == null and physical == null:
			# A key event that names no key — text an input method composed —
			# cannot be delivered again, so it is recorded like any other.
			entry["kind"] = "other"
			entry["class"] = event.get_class()
			return entry
		entry["kind"] = "key"
		if named != null:
			entry["key"] = named
		if physical != null:
			entry["physical"] = physical
		entry["unicode"] = key.unicode
		entry["pressed"] = key.pressed
		entry["echo"] = key.echo
		entry["modifiers"] = _modifiers(key)
	elif event is InputEventMouseButton:
		var button := event as InputEventMouseButton
		entry["kind"] = "mouse_button"
		entry["button"] = _button_name(button.button_index)
		entry["pressed"] = button.pressed
		entry["position"] = _xy(button.position)
		entry["double_click"] = button.double_click
	elif event is InputEventMouseMotion:
		var motion := event as InputEventMouseMotion
		entry["kind"] = "mouse_motion"
		entry["position"] = _xy(motion.position)
		entry["relative"] = _xy(motion.relative)
		entry["button_mask"] = int(motion.button_mask)
	elif event is InputEventJoypadButton:
		var joy := event as InputEventJoypadButton
		entry["kind"] = "joypad_button"
		entry["device"] = joy.device
		entry["button"] = int(joy.button_index)
		entry["pressed"] = joy.pressed
	elif event is InputEventJoypadMotion:
		var axis := event as InputEventJoypadMotion
		entry["kind"] = "joypad_motion"
		entry["device"] = axis.device
		entry["axis"] = int(axis.axis)
		entry["value"] = axis.axis_value
	elif event is InputEventAction:
		var action := event as InputEventAction
		entry["kind"] = "action"
		entry["action"] = String(action.action)
		entry["pressed"] = action.pressed
		entry["strength"] = action.strength
	else:
		entry["kind"] = "other"
		entry["class"] = event.get_class()
		return entry
	entry["actions"] = matched_actions(event)
	return entry


static func _refuse(index: int, reason: String) -> Dictionary:
	return {"error": "event %d: %s" % [index, reason]}


static func _flag(entry: Dictionary, key: String, fallback: bool):
	var value = entry.get(key, fallback)
	return value if typeof(value) == TYPE_BOOL else null


static func _number(entry: Dictionary, key: String, low: float, high: float, fallback = null):
	var value = entry.get(key, fallback)
	if typeof(value) != TYPE_INT and typeof(value) != TYPE_FLOAT:
		return null
	var number := float(value)
	if not is_finite(number) or number < low or number > high:
		return null
	return number


static func _position(value):
	if typeof(value) != TYPE_DICTIONARY or value.size() != 2:
		return null
	var x = _number(value, "x", -MAX_COORDINATE, MAX_COORDINATE)
	var y = _number(value, "y", -MAX_COORDINATE, MAX_COORDINATE)
	if x == null or y == null:
		return null
	return Vector2(x, y)


## Build the event recorded entry `index` describes. Returns {"event", "key"}
## (`key` names what the event holds while pressed, or ""), {"skip": true}
## for an `other` entry, or {"error": reason}.
static func build(entry, index: int) -> Dictionary:
	if typeof(entry) != TYPE_DICTIONARY:
		return _refuse(index, "an event is an object")
	var kind := str(entry.get("kind", ""))
	match kind:
		"key":
			var event := InputEventKey.new()
			var named := false
			for field in ["key", "physical"]:
				var name = entry.get(field)
				if name == null:
					continue
				var code: Key = OS.find_keycode_from_string(str(name))
				if typeof(name) != TYPE_STRING or code == KEY_NONE:
					return _refuse(index, "%s is not a key name" % str(name))
				named = true
				if field == "key":
					event.keycode = code
				else:
					event.physical_keycode = code
			if not named:
				return _refuse(index, "a key event names `key` or `physical`")
			var unicode = _number(entry, "unicode", 0, 1114111, 0)
			var pressed = _flag(entry, "pressed", true)
			var echo = _flag(entry, "echo", false)
			if unicode == null or pressed == null or echo == null:
				return _refuse(index, "a key event's unicode, pressed and echo are a code point and booleans")
			event.unicode = int(unicode)
			event.pressed = pressed
			event.echo = echo
			var modifiers = entry.get("modifiers", {})
			if typeof(modifiers) != TYPE_DICTIONARY:
				return _refuse(index, "`modifiers` maps shift, ctrl, alt and meta to booleans")
			for modifier in modifiers:
				if not str(modifier) in ["shift", "ctrl", "alt", "meta"] or typeof(modifiers[modifier]) != TYPE_BOOL:
					return _refuse(index, "`modifiers` maps shift, ctrl, alt and meta to booleans")
			event.shift_pressed = bool(modifiers.get("shift", false))
			event.ctrl_pressed = bool(modifiers.get("ctrl", false))
			event.alt_pressed = bool(modifiers.get("alt", false))
			event.meta_pressed = bool(modifiers.get("meta", false))
			var held: int = event.keycode if event.keycode != KEY_NONE else event.physical_keycode
			return {"event": event, "key": "key:%d" % held}
		"mouse_button":
			var event := InputEventMouseButton.new()
			var button = entry.get("button", "left")
			var index_of = MOUSE_BUTTONS.get(button) if typeof(button) == TYPE_STRING else null
			if index_of == null and (typeof(button) == TYPE_INT or typeof(button) == TYPE_FLOAT) \
					and float(button) == floor(float(button)) and float(button) >= 1 and float(button) <= 9:
				index_of = int(button)
			if index_of == null:
				return _refuse(index, "%s is not a mouse button" % str(button))
			var at = _position(entry.get("position"))
			var pressed = _flag(entry, "pressed", true)
			var twice = _flag(entry, "double_click", false)
			if at == null or pressed == null or twice == null:
				return _refuse(index, "a mouse button event needs `position` {x, y} and boolean pressed and double_click")
			event.button_index = index_of
			event.pressed = pressed
			event.double_click = twice
			event.position = at
			event.global_position = at
			return {"event": event, "key": "mouse:%s" % str(_button_name(index_of))}
		"mouse_motion":
			var event := InputEventMouseMotion.new()
			var at = _position(entry.get("position"))
			var relative = _position(entry.get("relative", {"x": 0, "y": 0}))
			var mask = _number(entry, "button_mask", 0, 511, 0)
			if at == null or relative == null or mask == null or float(mask) != floor(float(mask)):
				return _refuse(index, "a mouse motion needs `position` and `relative` {x, y} and a whole button_mask")
			event.position = at
			event.global_position = at
			event.relative = relative
			event.button_mask = int(mask)
			return {"event": event, "key": ""}
		"joypad_button":
			var event := InputEventJoypadButton.new()
			var device = _number(entry, "device", 0, MAX_DEVICE, 0)
			var button = _number(entry, "button", 0, MAX_JOY_BUTTON)
			var pressed = _flag(entry, "pressed", true)
			if device == null or button == null or pressed == null \
					or float(device) != floor(float(device)) or float(button) != floor(float(button)):
				return _refuse(index, "a joypad button event needs a whole device and button and a boolean pressed")
			event.device = int(device)
			event.button_index = int(button)
			event.pressed = pressed
			return {"event": event, "key": "joy:%d:%d" % [int(device), int(button)]}
		"joypad_motion":
			var event := InputEventJoypadMotion.new()
			var device = _number(entry, "device", 0, MAX_DEVICE, 0)
			var axis = _number(entry, "axis", 0, MAX_JOY_AXIS)
			var value = _number(entry, "value", -1.0, 1.0)
			if device == null or axis == null or value == null \
					or float(device) != floor(float(device)) or float(axis) != floor(float(axis)):
				return _refuse(index, "a joypad motion needs a whole device and axis and a value in [-1, 1]")
			event.device = int(device)
			event.axis = int(axis)
			event.axis_value = value
			return {"event": event, "key": "joyaxis:%d:%d" % [int(device), int(axis)]}
		"action":
			var event := InputEventAction.new()
			var action = entry.get("action")
			if typeof(action) != TYPE_STRING or not InputMap.has_action(action):
				return _refuse(index, "the game has no input action %s" % str(action))
			var pressed = _flag(entry, "pressed", true)
			var strength = _number(entry, "strength", 0.0, 1.0, 1.0)
			if pressed == null or strength == null:
				return _refuse(index, "an action event's pressed is a boolean and its strength in [0, 1]")
			event.action = action
			event.pressed = pressed
			event.strength = strength if pressed else 0.0
			return {"event": event, "key": "action:%s" % action}
		"other":
			return {"skip": true}
	return _refuse(index, "%s is not an input event kind" % kind)


## Whether `event` presses what it holds (a joypad axis away from rest
## counts as pressed).
static func presses(event: InputEvent) -> bool:
	if event is InputEventJoypadMotion:
		return absf((event as InputEventJoypadMotion).axis_value) > 0.0
	if event is InputEventMouseMotion:
		return false
	return event.is_pressed()
