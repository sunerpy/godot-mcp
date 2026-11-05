@tool
extends Node
## Presses one dialog button for `editor.dialog.respond` from this node's own
## frame, never from the godot_mcp plugin: a press can make the editor unload
## its plugins, as Reload Current Project does, and none of the plugin's code
## may be on the stack then. It frees itself once it ran.

## The press: the button's `emit_signal` bound to "pressed".
var action: Callable = Callable()
## Shared with the caller: its "outcome" stays "waiting" until the press ran,
## then is "pressed", or "gone" when the button was freed first.
var report: Dictionary = {}
var _ran := false


func _process(_delta: float) -> void:
	if _ran:
		return
	_ran = true
	set_process(false)
	if action.is_valid():
		report["outcome"] = "pressed"
		action.call()
	else:
		report["outcome"] = "gone"
	queue_free()
