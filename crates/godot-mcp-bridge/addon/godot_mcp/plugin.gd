@tool
extends EditorPlugin
## godot-mcp editor plugin: owns the bridge client and its status dock, and
## removes both when disabled, so enabling, reloading and disabling leave
## nothing behind in the editor.

const BridgeClient = preload("bridge_client.gd")
const Handlers = preload("editor_handlers.gd")
const StatusDock = preload("status_dock.gd")

var _client: Node = null
var _dock: Control = null


func _enter_tree() -> void:
	_client = BridgeClient.new()
	_client.name = "GodotMcpBridge"
	_client.handlers = Handlers.new(self)
	_client.client_version = Handlers.PLUGIN_VERSION
	add_child(_client)
	scene_saved.connect(_on_scene_saved)
	_dock = StatusDock.new()
	_dock.name = "GodotMcp"
	add_control_to_dock(DOCK_SLOT_RIGHT_BL, _dock)
	_client.status_changed.connect(_dock.show_status)
	_dock.show_status(_client.status())


func _on_scene_saved(path: String) -> void:
	var root := EditorInterface.get_edited_scene_root()
	if root != null and root.scene_file_path == path:
		_client.handlers.mark_saved(root)
	_client.send_event("editor.scene_saved", {"path": path})


func _exit_tree() -> void:
	if scene_saved.is_connected(_on_scene_saved):
		scene_saved.disconnect(_on_scene_saved)
	if _client != null:
		_client.close("the editor plugin is stopping")
		remove_child(_client)
		_client.free()
		_client = null
	if _dock != null:
		remove_control_from_docks(_dock)
		_dock.free()
		_dock = null
