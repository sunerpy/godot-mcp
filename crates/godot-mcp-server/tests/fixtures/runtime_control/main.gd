extends Node2D
## The runtime-control fixture: a small game the live runtime suite drives and
## watches. Everything is built here, so the scene file is one root node.
##
## * `Player` (a CharacterBody2D with a NavigationAgent2D) and three coins
##   stand on `Floor`, an L-shaped navigation region: a corridor along the top
##   and one down the left side, so a path between them turns a corner;
## * `Arena/Walker` (a CharacterBody3D) stands on a square 3D navigation mesh;
## * `Anchor` is put back where it started every physics frame;
## * `Elsewhere/Stray` lives in a SubViewport with a world of its own and no
##   navigation region;
## * the HUD holds buttons — one disabled, one hidden, one outside a headless
##   session's 64x64 viewport — and a status label;
## * the `Beat` timer emits `beat` every 0.2 s.
##
## The `*_in_ms` properties are delayed triggers: a test sets one and then
## waits, and the game changes on its own while the wait runs. Every press and
## release of `move_right` is also noted in `user://markers.log` and on stdout,
## where a test reads it while the probe is busy or after the game ended.

const Hero = preload("res://hero.gd")
const Anchor = preload("res://anchor.gd")

signal beat(count: int)

var beats := 0
var jumps := 0
var presses := 0
var toggles := 0
## How long `move_right` was last held, as the game measured it between its
## own press and release events, or -1 before the first release.
var last_hold_ms := -1
## The keys of the last 50 non-motion input events the game received.
var seen: Array[String] = []
## Set to N > 0 to add the node `Spawned` N ms later.
var spawn_in_ms := 0:
	set(value):
		spawn_in_ms = value
		_spawn_at = _deadline(value)
## Set to N > 0 to free the node `Doomed` N ms later.
var despawn_in_ms := 0:
	set(value):
		despawn_in_ms = value
		_despawn_at = _deadline(value)
## Set to N > 0 to show "Go!" on the status label N ms later.
var announce_in_ms := 0:
	set(value):
		announce_in_ms = value
		_announce_at = _deadline(value)
## Set to N > 0 to have the game itself deliver a tap of the E key N ms later.
var external_key_in_ms := 0:
	set(value):
		external_key_in_ms = value
		_external_at = _deadline(value)
## Pauses the scene tree.
var paused := false:
	set(value):
		paused = value
		get_tree().paused = value

var _hold_started := -1
var _spawn_at := -1
var _despawn_at := -1
var _announce_at := -1
var _external_at := -1
var _status: Label = null


static func _deadline(delay_ms: int) -> int:
	return Time.get_ticks_msec() + delay_ms if delay_ms > 0 else -1


func _ready() -> void:
	_add_actions()
	_build_world()
	_build_arena()
	_build_hud()
	var timer := Timer.new()
	timer.name = "Beat"
	timer.wait_time = 0.2
	timer.autostart = true
	timer.timeout.connect(_on_beat)
	add_child(timer)
	var doomed := Node2D.new()
	doomed.name = "Doomed"
	add_child(doomed)
	_note("fixture ready")


func _add_actions() -> void:
	for spec in [["jump", KEY_SPACE, JOY_BUTTON_A], ["move_right", KEY_D, JOY_BUTTON_DPAD_RIGHT]]:
		if InputMap.has_action(spec[0]):
			continue
		InputMap.add_action(spec[0])
		var key := InputEventKey.new()
		key.keycode = spec[1]
		InputMap.action_add_event(spec[0], key)
		var button := InputEventJoypadButton.new()
		button.button_index = spec[2]
		InputMap.action_add_event(spec[0], button)


func _build_world() -> void:
	var floor := NavigationRegion2D.new()
	floor.name = "Floor"
	var polygon := NavigationPolygon.new()
	polygon.vertices = PackedVector2Array([
		Vector2(0, 0), Vector2(400, 0), Vector2(400, 100),
		Vector2(100, 100), Vector2(100, 400), Vector2(0, 400),
	])
	polygon.add_polygon(PackedInt32Array([0, 1, 2, 3]))
	polygon.add_polygon(PackedInt32Array([0, 3, 4, 5]))
	floor.navigation_polygon = polygon
	add_child(floor)

	var hero: CharacterBody2D = Hero.new()
	hero.name = "Player"
	hero.position = Vector2(350, 50)
	hero.add_to_group("players")
	var shape := CollisionShape2D.new()
	shape.shape = CircleShape2D.new()
	hero.add_child(shape)
	var agent := NavigationAgent2D.new()
	agent.name = "Agent"
	hero.add_child(agent)
	add_child(hero)

	for spec in [["Coin1", Vector2(360, 60)], ["Coin2", Vector2(200, 50)], ["Coin3", Vector2(50, 350)]]:
		var coin := Area2D.new()
		coin.name = spec[0]
		coin.position = spec[1]
		coin.add_to_group("coins")
		add_child(coin)

	var anchor: Node2D = Anchor.new()
	anchor.name = "Anchor"
	anchor.position = Vector2(300, 50)
	add_child(anchor)

	var elsewhere := SubViewport.new()
	elsewhere.name = "Elsewhere"
	elsewhere.world_2d = World2D.new()
	elsewhere.size = Vector2i(8, 8)
	var stray := Node2D.new()
	stray.name = "Stray"
	elsewhere.add_child(stray)
	add_child(elsewhere)


func _build_arena() -> void:
	var arena := Node3D.new()
	arena.name = "Arena"
	add_child(arena)
	var floor := NavigationRegion3D.new()
	floor.name = "Floor3D"
	var mesh := NavigationMesh.new()
	mesh.vertices = PackedVector3Array([Vector3(0, 0, 0), Vector3(10, 0, 0), Vector3(10, 0, 10), Vector3(0, 0, 10)])
	mesh.add_polygon(PackedInt32Array([0, 1, 2, 3]))
	floor.navigation_mesh = mesh
	arena.add_child(floor)
	var walker := CharacterBody3D.new()
	walker.name = "Walker"
	walker.position = Vector3(1, 0, 1)
	var shape := CollisionShape3D.new()
	shape.shape = SphereShape3D.new()
	walker.add_child(shape)
	arena.add_child(walker)


func _button(parent: Node, name_: String, text: String, at: Vector2) -> Button:
	var button := Button.new()
	button.name = name_
	button.text = text
	button.position = at
	button.add_theme_font_size_override("font_size", 8)
	parent.add_child(button)
	return button


func _build_hud() -> void:
	var hud := CanvasLayer.new()
	hud.name = "HUD"
	add_child(hud)
	_status = Label.new()
	_status.name = "Status"
	_status.text = "Ready"
	_status.position = Vector2(0, 46)
	_status.add_theme_font_size_override("font_size", 8)
	hud.add_child(_status)
	_button(hud, "StartButton", "Start", Vector2(0, 0)).pressed.connect(_on_start)
	var toggle := _button(hud, "ToggleButton", "Toggle", Vector2(32, 0))
	toggle.toggle_mode = true
	toggle.toggled.connect(_on_toggled)
	_button(hud, "LockedButton", "Locked", Vector2(0, 22)).disabled = true
	_button(hud, "HiddenButton", "Hidden", Vector2(32, 22)).visible = false
	_button(hud, "FarButton", "Far", Vector2(600, 400))


func _on_start() -> void:
	presses += 1
	_status.text = "Started %d" % presses


func _on_toggled(_on: bool) -> void:
	toggles += 1


func _on_beat() -> void:
	beats += 1
	beat.emit(beats)


func _note(text: String) -> void:
	print("runtime-control: ", text)
	var path := "user://markers.log"
	var file := FileAccess.open(path, FileAccess.READ_WRITE if FileAccess.file_exists(path) else FileAccess.WRITE)
	if file != null:
		file.seek_end()
		file.store_line(text)
		file.close()


func _input(event: InputEvent) -> void:
	if event is InputEventMouseMotion:
		return
	seen.append(event.as_text())
	if seen.size() > 50:
		seen.pop_front()
	if event.is_action_pressed("jump"):
		jumps += 1
	if event.is_action_pressed("move_right"):
		if _hold_started < 0:
			_hold_started = Time.get_ticks_msec()
			_note("pressed move_right")
	elif event.is_action_released("move_right") and _hold_started >= 0:
		last_hold_ms = Time.get_ticks_msec() - _hold_started
		_hold_started = -1
		_note("released move_right after %d ms" % last_hold_ms)


func _process(_delta: float) -> void:
	var now := Time.get_ticks_msec()
	if _spawn_at >= 0 and now >= _spawn_at:
		_spawn_at = -1
		var spawned := Node2D.new()
		spawned.name = "Spawned"
		spawned.add_to_group("spawned")
		add_child(spawned)
	if _despawn_at >= 0 and now >= _despawn_at:
		_despawn_at = -1
		var doomed := get_node_or_null("Doomed")
		if doomed != null:
			doomed.queue_free()
	if _announce_at >= 0 and now >= _announce_at:
		_announce_at = -1
		_status.text = "Go!"
	if _external_at >= 0 and now >= _external_at:
		_external_at = -1
		for pressed in [true, false]:
			var key := InputEventKey.new()
			key.keycode = KEY_E
			key.physical_keycode = KEY_E
			key.pressed = pressed
			Input.parse_input_event(key)
