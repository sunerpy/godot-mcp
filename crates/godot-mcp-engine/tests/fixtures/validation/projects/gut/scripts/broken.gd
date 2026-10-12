extends Node

# The committed fake engine reports a parse error on whichever line carries the
# marker below, so the expected file/line comes from this file's real contents.
func _ready() -> void:
	var value := 1
	godot_mcp_calibration_syntax_error value
	print(value)
