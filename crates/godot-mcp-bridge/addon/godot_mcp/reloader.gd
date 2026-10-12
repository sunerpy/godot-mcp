@tool
extends Node
## Disables and re-enables the godot_mcp plugin from outside it, then frees
## itself. A plugin cannot reload itself while its own code is on the stack.

var plugin_directory := "godot_mcp"


func _ready() -> void:
	_reload.call_deferred()


func _reload() -> void:
	EditorInterface.set_plugin_enabled(plugin_directory, false)
	EditorInterface.set_plugin_enabled(plugin_directory, true)
	queue_free()
