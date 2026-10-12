@tool
extends Node2D

# A @tool script attached to a scene node. The official documentation's own
# warning applies verbatim: "the editor does not include protections for
# potential misuse of @tool scripts" and "Modifications in the editor are
# permanent, with no undo/redo possible".

var health := 100

func _process(_delta: float) -> void:
	if Engine.is_editor_hint():
		return
	health -= 1
