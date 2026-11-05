@tool
extends RefCounted
## `editor.dialogs.read` and `editor.dialog.respond`: the editor's visible
## dialogs, and pressing one button of one of them the way a click does.
##
## A dialog is an AcceptDialog, or one of its subclasses, anywhere in the
## editor's tree that is visible. Its id is its object id, which lasts as long
## as the dialog does and is never given to a later one. Its buttons are the
## visible buttons of its button row: `ok`, `cancel` and each custom button by
## the action name it reports to `custom_action`, or `button-N` when it has
## none.
##
## A press emits the button's `pressed` signal, which is what the dialog
## listens to for a click, from a helper node of its own (`dialog_press.gd`):
## a button can make the editor unload its plugins, as Reload Current Project
## does, and none of the plugin's code may be on the stack then. Nothing is
## ever pressed but the one button the caller names.

const DialogPress = preload("dialog_press.gd")

const DEFAULT_LIMIT := 32
const MAX_LIMIT := 64
const MAX_TITLE_CHARS := 512
const MAX_TEXT_CHARS := 4096
const MAX_BUTTONS := 16
const MAX_LABELS := 8
const MAX_NAME_CHARS := 256
## Frames the helper node gets to press the button.
const PRESS_FRAMES := 30
## Frames the dialog gets to close after the press.
const SETTLE_FRAMES := 3
## The largest object id: an id is a positive 64-bit integer.
const MAX_ID := "9223372036854775807"


static func _ok(payload: Dictionary) -> Dictionary:
	return {"ok": true, "payload": payload, "mutation_status": "committed"}


static func _fail(kind: String, message: String, status: String = "not_started") -> Dictionary:
	return {"ok": false, "error_kind": kind, "message": message, "mutation_status": status}


static func _tree() -> SceneTree:
	return EditorInterface.get_base_control().get_tree()


## Every visible dialog of the editor, in tree order.
static func visible_dialogs() -> Array:
	var found: Array = []
	for node in _tree().root.find_children("*", "AcceptDialog", true, false):
		if node is AcceptDialog and node.visible and node.is_inside_tree():
			found.append(node)
	return found


## What `dialog` says: its text, or when it has none, the text of the labels
## it shows.
static func text_of(dialog: AcceptDialog) -> String:
	if dialog.dialog_text != "":
		return dialog.dialog_text
	var parts: PackedStringArray = PackedStringArray()
	for label in dialog.find_children("*", "Label", true, false):
		if label is Label and label.is_visible_in_tree() and label.text != "":
			parts.append(label.text)
			if parts.size() >= MAX_LABELS:
				break
	return "\n".join(parts)


static func _printable(text: String) -> bool:
	if text == "" or text.length() > MAX_NAME_CHARS:
		return false
	for index in text.length():
		var code := text.unicode_at(index)
		if code < 0x20 or (code >= 0x7f and code <= 0x9f):
			return false
	return true


## The action a custom button reports to the dialog's `custom_action`, or "".
static func _custom_action(button: Button) -> String:
	for connection in button.get_signal_connection_list("pressed"):
		var callable: Callable = connection["callable"]
		if String(callable.get_method()).ends_with("_custom_action"):
			var bound: Array = callable.get_bound_arguments()
			if bound.size() == 1:
				return str(bound[0])
	return ""


## The visible buttons of `dialog`'s button row, as [{"button", "described"}].
static func _buttons(dialog: AcceptDialog) -> Array:
	var ok_button: Button = dialog.get_ok_button()
	var cancel_button: Button = dialog.get_cancel_button() if dialog is ConfirmationDialog else null
	var row: Node = ok_button.get_parent()
	var listed: Array = []
	var names: Dictionary = {}
	if row == null:
		return listed
	for child in row.get_children():
		if not (child is Button) or not child.visible:
			continue
		var role := "custom"
		var name := ""
		if child == ok_button:
			role = "ok"
			name = "ok"
		elif child == cancel_button:
			role = "cancel"
			name = "cancel"
		else:
			name = _custom_action(child)
		if not _printable(name) or names.has(name):
			name = "button-%d" % listed.size()
		names[name] = true
		listed.append({
			"button": child,
			"described": {"name": name, "role": role, "text": child.text, "disabled": child.disabled},
		})
		if listed.size() >= MAX_BUTTONS:
			break
	return listed


static func describe(dialog: AcceptDialog) -> Dictionary:
	var text := text_of(dialog)
	var buttons: Array = []
	for entry in _buttons(dialog):
		buttons.append(entry["described"])
	return {
		"id": str(dialog.get_instance_id()),
		"class": dialog.get_class(),
		"title": dialog.title.substr(0, MAX_TITLE_CHARS),
		"text": text.substr(0, MAX_TEXT_CHARS),
		"text_truncated": text.length() > MAX_TEXT_CHARS,
		"exclusive": dialog.exclusive,
		"buttons": buttons,
	}


## `editor.dialogs.read`: the visible dialogs, at most `limit`.
static func read(payload: Dictionary) -> Dictionary:
	var limit := int(payload.get("limit", DEFAULT_LIMIT))
	if limit < 1 or limit > MAX_LIMIT:
		return _fail("invalid_input", "`limit` is 1 to %d" % MAX_LIMIT)
	var shown := visible_dialogs()
	var dialogs: Array = []
	for dialog in shown:
		if dialogs.size() >= limit:
			break
		dialogs.append(describe(dialog))
	return {
		"ok": true,
		"payload": {"dialogs": dialogs, "count": shown.size(), "truncated": shown.size() > dialogs.size()},
		"mutation_status": "not_started",
	}


## The visible dialog whose id is `id`, or null.
static func _dialog(id: String):
	if not id.is_valid_int() or id.begins_with("-") or id.begins_with("+") or id.length() > MAX_ID.length():
		return null
	if id.length() == MAX_ID.length() and id > MAX_ID:
		return null
	var found = instance_from_id(id.to_int())
	if found is AcceptDialog and found.is_inside_tree() and found.visible:
		return found
	return null


## `editor.dialog.respond`: press the button named `button` of the dialog
## `dialog`, and answer what the dialog showed and whether it closed.
static func respond(payload: Dictionary) -> Dictionary:
	var id := str(payload.get("dialog", ""))
	var name := str(payload.get("button", ""))
	var dialog = _dialog(id)
	if dialog == null:
		return _fail("not_found", "dialog %s is not a visible dialog of this editor; read the dialogs again with editor.dialogs.get. Nothing was pressed" % id)
	var described := describe(dialog)
	var pressed = null
	var names: Array = []
	for entry in _buttons(dialog):
		names.append(entry["described"]["name"])
		if entry["described"]["name"] == name:
			pressed = entry
	if pressed == null:
		return _fail("not_found", "dialog %s (\"%s\") has no button named %s; its buttons are %s. Nothing was pressed" % [id, described["title"], name, ", ".join(names)])
	if pressed["described"]["disabled"]:
		return _fail("conflict", "the %s button of dialog %s (\"%s\") is disabled; nothing was pressed" % [name, id, described["title"]])
	# The helper writes what it did here; a dictionary is shared, not copied.
	var report := {"outcome": "waiting"}
	var press := Node.new()
	press.name = "GodotMcpDialogPress"
	press.set_script(DialogPress)
	press.set("action", Callable(pressed["button"], "emit_signal").bind("pressed"))
	press.set("report", report)
	EditorInterface.get_base_control().add_child(press)
	var helper := weakref(press)
	var tree := _tree()
	for frame in PRESS_FRAMES:
		await tree.process_frame
		if report["outcome"] != "waiting":
			break
	var outcome: String = str(report["outcome"])
	if outcome == "waiting":
		var waiting = helper.get_ref()
		if waiting != null:
			waiting.set("action", Callable())
			waiting.queue_free()
		return _fail("unavailable", "the editor did not run the press of the %s button of dialog %s within %d frames; nothing was pressed" % [name, id, PRESS_FRAMES])
	if outcome == "gone":
		return _fail("not_found", "dialog %s's %s button was freed before it could be pressed; nothing was pressed" % [id, name])
	for frame in SETTLE_FRAMES:
		await tree.process_frame
	var still: bool = is_instance_valid(dialog) and dialog.is_inside_tree() and dialog.visible
	var after: Array = []
	for shown in visible_dialogs():
		if after.size() >= DEFAULT_LIMIT:
			break
		after.append(describe(shown))
	return _ok({
		"dialog": id,
		"button": name,
		"title": described["title"],
		"text": described["text"],
		"closed": not still,
		"dialogs": after,
		"action": "godot-mcp: editor.dialog.respond",
	})
