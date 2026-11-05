@tool
extends VBoxContainer
## The "Godot MCP" bottom panel: the connection, the requests this editor
## answered, the tools the server serves, the bridge methods this add-on
## implements, and the user's controls.
##
## It draws a panel_state.gd object and changes it only through its controls:
## the "Pause agent edits" switch and the activity list's Clear button.
## "Reconnect now" calls the callable the plugin binds. Redraws are throttled
## and only the tab on screen is redrawn.

const PanelState = preload("panel_state.gd")
const REFRESH_SECONDS := 0.25
const STATUS_TAB := 0
const ACTIVITY_TAB := 1
const TOOLS_TAB := 2
const ADDON_TAB := 3
const CONTROLS_TAB := 4
const TAB_TITLES := ["Status", "Activity", "Tools", "Add-on", "Controls"]
## The tabs that show each part of the state.
const PART_TABS := {
	"status": [STATUS_TAB, ADDON_TAB],
	"activity": [ACTIVITY_TAB, STATUS_TAB],
	"tools": [TOOLS_TAB, STATUS_TAB],
	"controls": [CONTROLS_TAB, STATUS_TAB],
}
const STATUS_ROWS := [
	["connection", "Connection"],
	["edits", "Agent edits"],
	["counts", "Requests"],
	["last_request", "Last request"],
	["server", "Server"],
	["profile", "Profile"],
	["session", "Session"],
	["since", "Connected since"],
	["last_disconnect", "Last disconnect"],
	["addon", "Add-on"],
	["godot", "Godot"],
	["generation", "Project generation"],
]
const ACTIVITY_COLUMNS := ["Time", "Method", "Class", "Target", "Result", "Mutation", "ms"]
## The widest text each fixed-width activity column usually holds; "" marks a
## column that takes the remaining width, by the ratio in ACTIVITY_RATIOS.
const ACTIVITY_SAMPLES := ["00:00:00", "", "mutation", "", "unavailable", "not_started", "00000"]
const ACTIVITY_RATIOS := [1, 2, 1, 3, 1, 1, 1]
const PAUSED_BANNER := "Agent edits are paused"
const NEED_HEADINGS := {
	"project_files": "Project files",
	"headless_engine": "Headless Godot",
	"live_editor": "This editor",
	"game_session": "Managed game session",
	"editor_session": "Managed editor session",
}

var state = null
var _reconnect := Callable()
var _dirty := {}
var _last_refresh := -1.0
var _banner: PanelContainer = null
var _tabs: TabContainer = null
var _status_values := {}
var _activity_tree: Tree = null
var _activity_note: Label = null
var _tools_note: Label = null
var _tools_tree: Tree = null
## The tool groups the user opened, by need; every group starts closed.
var _open_groups := {}
var _drawing_tools := false
var _addon_tree: Tree = null
var _pause: CheckButton = null
var _controls_note: Label = null


func _init() -> void:
	name = "GodotMcpPanel"
	size_flags_vertical = Control.SIZE_EXPAND_FILL
	_banner = paused_banner("%s: every change the server asks this editor to make is refused." % PAUSED_BANNER)
	add_child(_banner)
	_tabs = TabContainer.new()
	_tabs.name = "Tabs"
	_tabs.size_flags_vertical = Control.SIZE_EXPAND_FILL
	add_child(_tabs)
	_tabs.add_child(_status_tab())
	_tabs.add_child(_activity_tab())
	_tabs.add_child(_tools_tab())
	_tabs.add_child(_addon_tab())
	_tabs.add_child(_controls_tab())
	for index in TAB_TITLES.size():
		_tabs.set_tab_title(index, TAB_TITLES[index])
	_tabs.tab_changed.connect(_on_tab_changed)
	for index in TAB_TITLES.size():
		_dirty[index] = true


## Draw `panel_state`, and call `reconnect` when the user asks to reconnect.
func bind(panel_state, reconnect: Callable) -> void:
	state = panel_state
	_reconnect = reconnect
	state.changed.connect(_on_state_changed)
	for index in TAB_TITLES.size():
		_dirty[index] = true
	_show_paused()


func _ready() -> void:
	if Engine.is_editor_hint():
		custom_minimum_size = Vector2(0, 180 * EditorInterface.get_editor_scale())
	style_paused_banner(_banner)
	_size_columns()
	set_process(true)


func _notification(what: int) -> void:
	if what == NOTIFICATION_VISIBILITY_CHANGED and is_visible_in_tree():
		_last_refresh = -1.0


func _on_state_changed(part: String) -> void:
	for index in PART_TABS.get(part, []):
		_dirty[index] = true
	if part == "controls":
		_show_paused()


func _show_paused() -> void:
	if state == null:
		return
	var paused: bool = state.mutations_paused()
	_banner.visible = paused
	_pause.set_pressed_no_signal(paused)


func _on_tab_changed(_tab: int) -> void:
	_last_refresh = -1.0


func _process(_delta: float) -> void:
	if state == null or _dirty.is_empty() or not is_visible_in_tree():
		return
	var now := Time.get_ticks_msec() / 1000.0
	if _last_refresh >= 0.0 and now - _last_refresh < REFRESH_SECONDS:
		return
	_last_refresh = now
	var tab := _tabs.current_tab
	if _dirty.has(tab):
		_dirty.erase(tab)
		_refresh_tab(tab)


## Redraw every tab now, whether or not the panel is on screen.
func refresh() -> void:
	if state == null:
		return
	for index in TAB_TITLES.size():
		_refresh_tab(index)
	_dirty.clear()


func _refresh_tab(tab: int) -> void:
	match tab:
		STATUS_TAB:
			_refresh_status()
		ACTIVITY_TAB:
			_refresh_activity()
		TOOLS_TAB:
			_refresh_tools()
		ADDON_TAB:
			_refresh_addon()
		CONTROLS_TAB:
			_show_paused()


## What the panel shows, for a test to read: every tab redrawn first.
func snapshot() -> Dictionary:
	refresh()
	var status := {}
	for key in _status_values.keys():
		status[key] = _status_values[key].text
	var groups := 0
	var tools := 0
	var tools_root := _tools_tree.get_root()
	if tools_root != null:
		for group in tools_root.get_children():
			groups += 1
			tools += group.get_child_count()
	var rows: Array = []
	var activity_root := _activity_tree.get_root()
	if activity_root != null:
		for item in activity_root.get_children():
			var columns: Array = []
			for column in ACTIVITY_COLUMNS.size():
				columns.append(item.get_text(column))
			rows.append(columns)
	var addon := 0
	var addon_root := _addon_tree.get_root()
	if addon_root != null:
		for group in addon_root.get_children():
			addon += group.get_child_count()
	var titles: Array = []
	for index in _tabs.get_tab_count():
		titles.append(_tabs.get_tab_title(index))
	return {
		"tabs": titles,
		"status": status,
		"activity": rows,
		"activity_note": _activity_note.text,
		"tools_state": state.tools_state(),
		"tools_note": _tools_note.text,
		"tool_groups": groups,
		"tools": tools,
		"addon_methods": addon,
		"paused": _pause.button_pressed,
		"paused_banner": _banner.visible,
		"served": state.served,
		"failed": state.failed,
		"refused": state.refused,
		"server_status_ticks_msec": state.server_status_ticks_msec,
		"first_status_ticks_msec": state.first_status_ticks_msec,
	}


# --- building the tabs -----------------------------------------------------------


## A banner saying agent edits are paused, hidden until they are; the dock
## shows one too. Style it with style_paused_banner() once it is in the tree.
static func paused_banner(text: String) -> PanelContainer:
	var banner := PanelContainer.new()
	banner.name = "PausedBanner"
	banner.visible = false
	var label := Label.new()
	label.text = text
	label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	banner.add_child(label)
	return banner


## Give `banner` a background in the editor theme's error color.
static func style_paused_banner(banner: Control) -> void:
	var color := Color(0.55, 0.12, 0.1)
	if banner.is_inside_tree() and banner.has_theme_color("error_color", "Editor"):
		color = banner.get_theme_color("error_color", "Editor").darkened(0.45)
	var box := StyleBoxFlat.new()
	box.bg_color = color
	box.set_corner_radius_all(3)
	box.content_margin_top = 4
	box.content_margin_bottom = 4
	box.content_margin_left = 6
	box.content_margin_right = 6
	banner.add_theme_stylebox_override("panel", box)


static func _wrapping_label(text: String = "") -> Label:
	var label := Label.new()
	label.text = text
	label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	label.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	return label


## A vertical scroll area whose content is as wide as the area, so wrapped
## text wraps at its width and a tall tab never makes the panel taller.
static func _scrolling(tab_name: String, content: Control) -> ScrollContainer:
	var scroll := ScrollContainer.new()
	scroll.name = tab_name
	scroll.horizontal_scroll_mode = ScrollContainer.SCROLL_MODE_DISABLED
	content.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	scroll.add_child(content)
	return scroll


func _status_tab() -> Control:
	var grid := GridContainer.new()
	grid.name = "Rows"
	grid.columns = 2
	for row in STATUS_ROWS:
		var title := Label.new()
		title.text = row[1]
		grid.add_child(title)
		var value := _wrapping_label("-")
		value.name = row[0]
		grid.add_child(value)
		_status_values[row[0]] = value
	return _scrolling("Status", grid)


func _activity_tab() -> Control:
	var box := VBoxContainer.new()
	box.name = "Activity"
	var bar := HBoxContainer.new()
	box.add_child(bar)
	var clear := Button.new()
	clear.name = "Clear"
	clear.text = "Clear"
	clear.tooltip_text = "Empty the list. The counts on the Status tab are kept."
	clear.pressed.connect(_on_clear)
	bar.add_child(clear)
	var copy := Button.new()
	copy.name = "Copy"
	copy.text = "Copy"
	copy.tooltip_text = "Copy the list as a plain-text table, oldest row first."
	copy.pressed.connect(_on_copy_activity)
	bar.add_child(copy)
	_activity_note = _wrapping_label()
	bar.add_child(_activity_note)
	_activity_tree = Tree.new()
	_activity_tree.name = "Rows"
	_activity_tree.size_flags_vertical = Control.SIZE_EXPAND_FILL
	_activity_tree.columns = ACTIVITY_COLUMNS.size()
	_activity_tree.column_titles_visible = true
	_activity_tree.hide_root = true
	_activity_tree.select_mode = Tree.SELECT_ROW
	for column in ACTIVITY_COLUMNS.size():
		_activity_tree.set_column_title(column, ACTIVITY_COLUMNS[column])
		_activity_tree.set_column_expand(column, ACTIVITY_SAMPLES[column] == "")
		_activity_tree.set_column_expand_ratio(column, ACTIVITY_RATIOS[column])
	box.add_child(_activity_tree)
	return box


## Give every fixed-width column the width of its usual text in the editor's
## font; done once the trees are in the tree, where they have a theme.
func _size_columns() -> void:
	var tree_font := _activity_tree.get_theme_font("font")
	var font_size := _activity_tree.get_theme_font_size("font_size")
	if tree_font == null:
		return
	# A cell's own margins and separation, which the font does not measure.
	var margin := 28.0 * (EditorInterface.get_editor_scale() if Engine.is_editor_hint() else 1.0)
	for column in ACTIVITY_COLUMNS.size():
		var sample: String = ACTIVITY_SAMPLES[column]
		if sample == "":
			continue
		var width := maxf(tree_font.get_string_size(sample, HORIZONTAL_ALIGNMENT_LEFT, -1, font_size).x,
			tree_font.get_string_size(ACTIVITY_COLUMNS[column], HORIZONTAL_ALIGNMENT_LEFT, -1, font_size).x)
		_activity_tree.set_column_custom_minimum_width(column, int(width + margin))
	var kind := maxf(tree_font.get_string_size("changes", HORIZONTAL_ALIGNMENT_LEFT, -1, font_size).x,
		tree_font.get_string_size("Kind", HORIZONTAL_ALIGNMENT_LEFT, -1, font_size).x)
	_tools_tree.set_column_custom_minimum_width(2, int(kind + margin))


func _tools_tab() -> Control:
	var box := VBoxContainer.new()
	box.name = "Tools"
	_tools_note = _wrapping_label()
	box.add_child(_tools_note)
	_tools_tree = Tree.new()
	_tools_tree.name = "Groups"
	_tools_tree.size_flags_vertical = Control.SIZE_EXPAND_FILL
	_tools_tree.columns = 3
	_tools_tree.column_titles_visible = true
	_tools_tree.hide_root = true
	_tools_tree.set_column_title(0, "Tool")
	_tools_tree.set_column_title(1, "Description")
	_tools_tree.set_column_title(2, "Kind")
	_tools_tree.set_column_expand_ratio(0, 2)
	_tools_tree.set_column_expand_ratio(1, 3)
	_tools_tree.set_column_expand(2, false)
	_tools_tree.item_collapsed.connect(_on_tool_group_toggled)
	box.add_child(_tools_tree)
	return box


func _addon_tab() -> Control:
	var box := VBoxContainer.new()
	box.name = "Add-on"
	box.add_child(_wrapping_label("The bridge methods this add-on serves. A read never changes the editor; a mutation does, and is refused while agent edits are paused."))
	_addon_tree = Tree.new()
	_addon_tree.name = "Methods"
	_addon_tree.size_flags_vertical = Control.SIZE_EXPAND_FILL
	_addon_tree.hide_root = true
	box.add_child(_addon_tree)
	return box


func _controls_tab() -> Control:
	var box := VBoxContainer.new()
	box.name = "Box"
	_pause = CheckButton.new()
	_pause.name = "PauseAgentEdits"
	_pause.text = "Pause agent edits"
	_pause.size_flags_horizontal = Control.SIZE_SHRINK_BEGIN
	_pause.toggled.connect(_on_pause_toggled)
	box.add_child(_pause)
	var buttons := HBoxContainer.new()
	box.add_child(buttons)
	var reconnect := Button.new()
	reconnect.name = "ReconnectNow"
	reconnect.text = "Reconnect now"
	reconnect.tooltip_text = "Drop the connection to the godot-mcp server and dial it again at once."
	reconnect.pressed.connect(_on_reconnect)
	buttons.add_child(reconnect)
	var copy := Button.new()
	copy.name = "CopyDiagnostics"
	copy.text = "Copy diagnostics"
	copy.tooltip_text = "Copy versions, the connection, the session, the counts and the last errors. No secret is copied."
	copy.pressed.connect(_on_copy_diagnostics)
	buttons.add_child(copy)
	_controls_note = _wrapping_label()
	buttons.add_child(_controls_note)
	box.add_child(_wrapping_label("While paused, this editor refuses every change the server asks it to make, before the change starts, as unavailable and not started. Reads still work, and a change already running finishes. The server's own file writes, such as file and script patches or a scene batch on a scene this editor does not hold, do not pass through the editor and are not paused. The switch lasts until it is turned off, the editor closes or the add-on reloads. It is a convenience, not a security boundary: any program running as you can still change the project's files."))
	return _scrolling("Controls", box)


# --- drawing ---------------------------------------------------------------------


func _theme_color(color_name: String, fallback: Color) -> Color:
	if is_inside_tree() and has_theme_color(color_name, "Editor"):
		return get_theme_color(color_name, "Editor")
	return fallback


func _set_status(key: String, text: String) -> void:
	_status_values[key].text = text


func _refresh_status() -> void:
	_set_status("connection", state.connection if state.connection != "" else "starting")
	_set_status("edits", "PAUSED by you: every change the server asks this editor to make is refused" if state.mutations_paused() else "allowed")
	_set_status("counts", "%d served, %d failed, %d refused" % [state.served, state.failed, state.refused])
	_set_status("last_request", PanelState.moment(state.last_request_unix_ms))
	_set_status("server", state.server_version if state.server_version != "" else "-")
	var profile := "-"
	if not state.server_status.is_empty():
		profile = state.server_status["profile"]
		if state.server_status["transport"] != "":
			profile += ", over %s" % state.server_status["transport"]
	_set_status("profile", profile)
	_set_status("session", state.session_id if state.session_id != "" else "-")
	_set_status("since", PanelState.moment(state.connected_since_unix_ms) if state.connected else "not connected")
	_set_status("last_disconnect", state.last_disconnect if state.last_disconnect != "" else "none")
	_set_status("addon", state.addon_version if state.addon_version != "" else "-")
	_set_status("godot", state.godot_version if state.godot_version != "" else "-")
	_set_status("generation", state.project_generation if state.project_generation != "" else "-")


func _refresh_activity() -> void:
	_activity_tree.clear()
	var root := _activity_tree.create_item()
	var attention := _theme_color("error_color", Color(1.0, 0.45, 0.4))
	var refused := _theme_color("warning_color", Color(1.0, 0.8, 0.3))
	for index in range(state.activity.size() - 1, -1, -1):
		var row: Dictionary = state.activity[index]
		var item := _activity_tree.create_item(root)
		var result: String = row["result"]
		if not row["delivered"]:
			result += ", not sent"
		var texts := [
			PanelState.clock(int(row["time_unix_ms"])),
			row["method"],
			row["class"],
			row["target"],
			result,
			row["mutation_status"],
			str(row["duration_ms"]) if int(row["duration_ms"]) >= 0 else "-",
		]
		for column in texts.size():
			item.set_text(column, texts[column])
			item.set_tooltip_text(column, texts[column])
		if row["attention"]:
			for column in texts.size():
				item.set_custom_color(column, attention)
		elif row["refused"]:
			item.set_custom_color(4, refused)
	var note := "%d requests listed, newest first" % state.activity.size()
	if state.dropped > 0:
		note += "; %d older ones dropped" % state.dropped
	_activity_note.text = note


func _availability(group: Dictionary) -> String:
	var text: String = group["status"]
	if group["reason"] != "":
		text += ": %s" % group["reason"]
	return text


func _refresh_tools() -> void:
	_drawing_tools = true
	_tools_tree.clear()
	_drawing_tools = false
	match state.tools_state():
		"not_connected":
			_tools_note.text = "Not connected to the godot-mcp server."
			return
		"not_described":
			_tools_note.text = "The server did not describe its tools. It may be older than this add-on."
			return
		"waiting":
			var waiting := "Waiting for the server to describe its tools."
			if state.server_status_refusal != "":
				waiting += " Its last description was refused: %s" % state.server_status_refusal
			_tools_note.text = waiting
			return
	var status: Dictionary = state.server_status
	var note := "%d tools in the %s profile of godot-mcp %s, by what they need; open a group to list them" % [status["tools"].size(), status["profile"], status["server_version"]]
	if status["tools_truncated"]:
		note += ". The server has %d; the first %d are listed" % [status["tools_total"], status["tools"].size()]
	if status["mutations_blocked"] != "":
		note += ". The server blocks every change: %s" % status["mutations_blocked"]
	_tools_note.text = note + "."
	var ready := _theme_color("success_color", Color(0.45, 0.85, 0.5))
	var not_ready := _theme_color("warning_color", Color(1.0, 0.8, 0.3))
	_drawing_tools = true
	var root := _tools_tree.create_item()
	for group in state.tool_groups():
		var need: String = group["needs"]
		var heading := _tools_tree.create_item(root)
		heading.set_metadata(0, need)
		heading.set_text(0, "%s (%d)" % [NEED_HEADINGS.get(need, need), group["tools"].size()])
		heading.set_text(1, _availability(group))
		heading.set_tooltip_text(1, _availability(group))
		heading.set_custom_color(1, ready if group["status"] == "ready" else not_ready)
		for tool in group["tools"]:
			var item := _tools_tree.create_item(heading)
			item.set_text(0, tool["name"])
			item.set_text(1, tool["title"])
			item.set_tooltip_text(1, tool["title"])
			item.set_text(2, "read" if tool["read_only"] else "changes")
		heading.collapsed = not _open_groups.get(need, false)
	_drawing_tools = false


func _on_tool_group_toggled(item: TreeItem) -> void:
	if _drawing_tools:
		return
	var need = item.get_metadata(0)
	if typeof(need) == TYPE_STRING:
		_open_groups[need] = not item.collapsed


func _refresh_addon() -> void:
	_addon_tree.clear()
	var root := _addon_tree.create_item()
	var session: Array = []
	var reads: Array = []
	for method in state.addon_reads:
		if str(method).begins_with("bridge."):
			session.append(method)
		else:
			reads.append(method)
	for group in [["Session", session], ["Reads", reads], ["Mutations", state.addon_mutations]]:
		var heading := _addon_tree.create_item(root)
		heading.set_text(0, "%s (%d)" % [group[0], group[1].size()])
		for method in group[1]:
			_addon_tree.create_item(heading).set_text(0, method)


# --- controls --------------------------------------------------------------------


func _on_pause_toggled(pressed: bool) -> void:
	if state != null:
		state.set_paused(pressed)


func _on_reconnect() -> void:
	if _reconnect.is_valid():
		_reconnect.call()


func _on_clear() -> void:
	if state != null:
		state.clear_activity()


static func _copy(text: String) -> bool:
	if not DisplayServer.has_feature(DisplayServer.FEATURE_CLIPBOARD):
		return false
	DisplayServer.clipboard_set(text)
	return true


func _on_copy_activity() -> void:
	if state == null:
		return
	if _copy(state.activity_text()):
		_activity_note.text = "Copied %d rows." % state.activity.size()
	else:
		_activity_note.text = "This editor has no clipboard."


func _on_copy_diagnostics() -> void:
	if state == null:
		return
	_controls_note.text = "Diagnostics copied." if _copy(state.diagnostics_text()) else "This editor has no clipboard."
