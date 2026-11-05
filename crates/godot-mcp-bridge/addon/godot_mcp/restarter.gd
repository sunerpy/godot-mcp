@tool
extends Node
## Restarts the editor for `editor.restart` from outside the godot_mcp plugin:
## the editor unloads its plugins inside EditorInterface.restart_editor(), so
## none of the plugin's code may be on the stack when it is called. It frees
## itself once it is done.
##
## It waits until the answer to the restart request has left the socket, then
## asks the editor to restart. An editor with nothing to ask unloads its
## plugins and exits, and starts the restarted editor as it exits. An editor
## that still holds something unsaved asks what to do instead: every dialog it
## opened is cancelled, so nothing is saved or discarded, and the decline is
## reported through the handler set, whose editor.state says so.

const EditorDialogs = preload("editor_dialogs.gd")

const PLUGIN := "godot_mcp"
## Frames the answer gets to leave the socket, at most.
const FLUSH_FRAMES := 120

var handlers_ref: WeakRef = null
var client_ref: WeakRef = null


func _ready() -> void:
	_run.call_deferred()


func _run() -> void:
	var tree := get_tree()
	for frame in FLUSH_FRAMES:
		await tree.process_frame
		var client = client_ref.get_ref() if client_ref != null else null
		if frame >= 1 and (client == null or client.outbound_pending() == 0):
			break
	var before: Dictionary = {}
	for dialog in EditorDialogs.visible_dialogs():
		before[dialog.get_instance_id()] = true
	EditorInterface.restart_editor(false)
	var handlers = handlers_ref.get_ref() if handlers_ref != null else null
	if handlers == null or not EditorInterface.is_plugin_enabled(PLUGIN):
		# The editor unloaded its plugins: it exits now and starts again.
		queue_free()
		return
	var question := {"title": "", "text": ""}
	for dialog in EditorDialogs.visible_dialogs():
		if before.has(dialog.get_instance_id()):
			continue
		if question["title"] == "" and question["text"] == "":
			question = {"title": dialog.title, "text": EditorDialogs.text_of(dialog)}
		if dialog is ConfirmationDialog:
			dialog.get_cancel_button().pressed.emit()
		else:
			dialog.hide()
	handlers.restart_declined(question)
	queue_free()
