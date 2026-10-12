@tool
extends VBoxContainer
## A small dock showing whether the godot-mcp server is connected.

var _label: Label = null


func _init() -> void:
	_label = Label.new()
	_label.text = "godot-mcp: starting"
	_label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	add_child(_label)


func show_status(text: String) -> void:
	if _label != null:
		_label.text = "godot-mcp: %s" % (text if text != "" else "starting")
