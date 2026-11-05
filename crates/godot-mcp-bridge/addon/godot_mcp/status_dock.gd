@tool
extends VBoxContainer
## The godot_mcp dock: whether the godot-mcp server is connected and whether
## agent edits are paused, with the pause switch, "Reconnect now" and a way to
## the "Godot MCP" bottom panel.

const McpPanel = preload("panel.gd")

var state = null
var _reconnect := Callable()
var _open_panel := Callable()
var _status: Label = null
var _banner: PanelContainer = null
var _pause: CheckButton = null


func _init() -> void:
	_status = Label.new()
	_status.name = "Status"
	_status.text = "godot-mcp: starting"
	_status.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	add_child(_status)
	_banner = McpPanel.paused_banner(McpPanel.PAUSED_BANNER)
	add_child(_banner)
	_pause = CheckButton.new()
	_pause.name = "PauseAgentEdits"
	_pause.text = "Pause agent edits"
	_pause.tooltip_text = "Refuse every change the server asks this editor to make, until this is turned off. Reads still work."
	_pause.toggled.connect(_on_pause_toggled)
	add_child(_pause)
	var row := HBoxContainer.new()
	add_child(row)
	var reconnect := Button.new()
	reconnect.name = "ReconnectNow"
	reconnect.text = "Reconnect now"
	reconnect.pressed.connect(_on_reconnect)
	row.add_child(reconnect)
	var open := Button.new()
	open.name = "OpenPanel"
	open.text = "Details"
	open.tooltip_text = "Show the Godot MCP panel."
	open.pressed.connect(_on_open_panel)
	row.add_child(open)


## Show `panel_state`; call `reconnect` and `open_panel` from the buttons.
func bind(panel_state, reconnect: Callable, open_panel: Callable) -> void:
	state = panel_state
	_reconnect = reconnect
	_open_panel = open_panel
	state.changed.connect(_on_state_changed)
	_show()


func _ready() -> void:
	McpPanel.style_paused_banner(_banner)


func _on_state_changed(part: String) -> void:
	if part == "status" or part == "controls":
		_show()


func _show() -> void:
	if state == null:
		return
	var text: String = state.connection if state.connection != "" else "starting"
	_status.text = "godot-mcp: %s" % text
	var paused: bool = state.mutations_paused()
	_banner.visible = paused
	_pause.set_pressed_no_signal(paused)


## What the dock shows, for a test to read.
func snapshot() -> Dictionary:
	_show()
	return {"status": _status.text, "paused_banner": _banner.visible, "paused": _pause.button_pressed}


## Turn the pause switch, as a user clicking it does.
func set_paused(paused: bool) -> void:
	_pause.button_pressed = paused


func _on_pause_toggled(pressed: bool) -> void:
	if state != null:
		state.set_paused(pressed)


func _on_reconnect() -> void:
	if _reconnect.is_valid():
		_reconnect.call()


func _on_open_panel() -> void:
	if _open_panel.is_valid():
		_open_panel.call()
