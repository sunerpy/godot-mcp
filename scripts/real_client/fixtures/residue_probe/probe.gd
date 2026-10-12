@tool
extends EditorPlugin
## Test fixture, never shipped. Inside a real editor it enables the godot_mcp
## plugin, waits until it is connected to the server, disables it, enables it
## again (a reload), disables it again, and prints what stayed behind after
## each disable compared with before the first enable: editor nodes, orphan
## nodes and the add-on's own named nodes. Then it quits the editor.

const TARGET := "godot_mcp"
const TIMEOUT_FRAMES := 3000

var _step := 0
var _frames := 0
var _report := {}
var _baseline := {}


func _fingerprint() -> Dictionary:
	var root := EditorInterface.get_base_control().get_tree().root
	return {
		"nodes": root.get_tree().get_node_count(),
		"orphans": int(Performance.get_monitor(Performance.OBJECT_ORPHAN_NODE_COUNT)),
		"bridge_nodes": root.find_children("GodotMcpBridge", "", true, false).size(),
		"dock_nodes": root.find_children("GodotMcp", "", true, false).size(),
		"reloader_nodes": root.find_children("GodotMcpReloader", "", true, false).size(),
		"enabled": EditorInterface.is_plugin_enabled(TARGET),
	}


func _connected() -> bool:
	var root := EditorInterface.get_base_control().get_tree().root
	for node in root.find_children("GodotMcpBridge", "", true, false):
		if node.has_method("is_connected_to_server") and node.is_connected_to_server():
			return true
	return false


func _process(_delta: float) -> void:
	_frames += 1
	if _frames > TIMEOUT_FRAMES:
		_report["timed_out_at_step"] = _step
		_finish()
		return
	match _step:
		0:
			if _frames > 30:
				_baseline = _fingerprint()
				_report["baseline"] = _baseline
				EditorInterface.set_plugin_enabled(TARGET, true)
				_step = 1
		1:
			if _connected():
				_report["enabled_once"] = _fingerprint()
				EditorInterface.set_plugin_enabled(TARGET, false)
				_step = 2
				_frames_at = _frames
		2:
			if _frames - _frames_at > 30:
				_report["disabled_once"] = _fingerprint()
				EditorInterface.set_plugin_enabled(TARGET, true)
				_step = 3
		3:
			if _connected():
				_report["enabled_twice"] = _fingerprint()
				EditorInterface.set_plugin_enabled(TARGET, false)
				_step = 4
				_frames_at = _frames
		4:
			if _frames - _frames_at > 30:
				_report["disabled_twice"] = _fingerprint()
				_finish()

var _frames_at := 0


func _finish() -> void:
	print("RESIDUE_PROBE ", JSON.stringify(_report))
	_step = 99
	EditorInterface.get_base_control().get_tree().quit(0)
