@tool
extends Node

# A @tool autoload. Per the official documentation, a @tool script "makes it
# execute in the editor", and an autoload is instantiated as soon as the project
# is loaded — so the body of _ready() runs during an ordinary `--import`, before
# anyone has pressed "play".
#
# This fixture's body is inert on purpose: the point being demonstrated is that
# code runs at all, not that it does something spectacular.

func _ready() -> void:
	if Engine.is_editor_hint():
		print("godot-mcp fixture: autoload ran inside the editor")
