@tool
extends EditorPlugin
## godot-mcp editor plugin: owns the bridge client, its status dock, the
## "Godot MCP" bottom panel and the log capture, and removes all of them when
## disabled, so enabling, reloading and disabling leave nothing behind in the
## editor.

const BridgeClient = preload("bridge_client.gd")
const EditorLog = preload("editor_log.gd")
const Handlers = preload("editor_handlers.gd")
const McpPanel = preload("panel.gd")
const PanelState = preload("panel_state.gd")
const StatusDock = preload("status_dock.gd")

var _client: Node = null
var _dock: Control = null
var _panel: Control = null
var _state = null
var _log = null


func _enter_tree() -> void:
	# Capture starts before the bridge dials, so the session it describes
	# already says whether the log can be read.
	_log = EditorLog.new()
	_log.start()
	_state = PanelState.new()
	var handlers = Handlers.new(self)
	handlers.editor_log = _log
	handlers.panel_state = _state
	var reads: Array = []
	var mutations: Array = []
	for method in handlers.methods():
		if handlers.is_mutation(method):
			mutations.append(method)
		else:
			reads.append(method)
	_state.set_addon(Handlers.PLUGIN_VERSION, reads, mutations)
	_client = BridgeClient.new()
	_client.name = "GodotMcpBridge"
	_client.handlers = handlers
	# The dispatcher asks the panel's pause switch before every mutation.
	_client.mutation_gate = _state
	_client.client_version = Handlers.PLUGIN_VERSION
	_client.report_late_answers = true
	_client.status_changed.connect(_state.on_status)
	_client.disconnected.connect(_state.on_disconnected)
	_client.session_opened.connect(_state.on_session_opened)
	_client.request_answered.connect(_state.on_request_answered)
	add_child(_client)
	scene_saved.connect(_on_scene_saved)
	_panel = McpPanel.new()
	_panel.bind(_state, _client.reconnect_now)
	add_control_to_bottom_panel(_panel, "Godot MCP")
	_dock = StatusDock.new()
	_dock.name = "GodotMcp"
	_dock.bind(_state, _client.reconnect_now, _show_panel)
	add_control_to_dock(DOCK_SLOT_RIGHT_BL, _dock)
	_state.on_status(_client.status())


func _show_panel() -> void:
	if _panel != null:
		make_bottom_panel_item_visible(_panel)


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
	if _panel != null:
		remove_control_from_bottom_panel(_panel)
		_panel.free()
		_panel = null
	_state = null
	if _log != null:
		_log.stop()
		_log = null
