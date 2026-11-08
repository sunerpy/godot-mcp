extends Node2D
## The frame-stepping fixture: a scene that counts and records what runs in
## every physics and idle frame, so the live suite can say exactly what a step
## advanced and in which physics frame replayed input reached the game.
##
## * `Faller` falls freely under gravity, with no damping and nothing to hit:
##   its speed counts the steps the physics server took (`faller_steps`);
## * `Always` processes while the tree is paused;
## * `Tick`, a Timer counting physics time, and a SceneTreeTimer re-armed
##   every 50 ms count how far each kind of timer ran;
## * the `jump` action (Space or joypad A) is recorded as the game sees it:
##   in which physics frames `Input.is_action_just_pressed` and
##   `is_action_just_released` held, in which it became pressed and released,
##   and when `_input` and `_unhandled_input` received each jump event.
##
## `pause_after_physics` and `stall_ms` are triggers a test sets; `paused`
## pauses the scene tree as game code would. Pause changes are noted in
## `user://markers.log` and on stdout.

const MAX_RECORDED := 2000

## A node that processes while the scene tree is paused.
class AlwaysCounter extends Node:
	var physics_calls := 0

	func _physics_process(_delta: float) -> void:
		physics_calls += 1


var physics_calls := 0
var process_calls := 0
## Engine.get_physics_frames() in each of the last MAX_RECORDED physics frames
## this node processed.
var physics_frames: Array[int] = []
## The physics frame this node last processed, and the one in which the
## SceneTree's physics_frame signal last reached it.
var last_physics_frame := -1
var last_signal_frame := -1
var signal_calls := 0
var timer_fires := 0
var scene_timer_fires := 0
## The jump action as the game saw it, by physics frame.
var just_pressed: Array[int] = []
var just_released: Array[int] = []
var pressed_from: Array[int] = []
var released_from: Array[int] = []
## Every jump event `_input` and `_unhandled_input` received:
## [physics frame counter, whether in a physics frame, pressed].
var input_seen: Array = []
var unhandled_seen: Array = []
## Set N > 0 to have the game pause itself in the N-th physics frame it
## processes from now.
var pause_after_physics := 0
## Set N > 0 to block every idle frame for N ms, so that physics frames run
## in bursts to catch up.
var stall_ms := 0
## Pauses the scene tree, as game code would.
var paused := false:
	set(value):
		paused = value
		get_tree().paused = value
## How many steps the physics server took for `Faller`, from its speed.
var faller_steps: int:
	get:
		if _faller == null:
			return -1
		var speed: Vector2 = PhysicsServer2D.body_get_state(_faller.get_rid(), PhysicsServer2D.BODY_STATE_LINEAR_VELOCITY)
		var gravity := float(ProjectSettings.get_setting("physics/2d/default_gravity"))
		return roundi(speed.y / (gravity / float(Engine.physics_ticks_per_second)))

var _faller: RigidBody2D = null
var _was_pressed := false


func _ready() -> void:
	_add_actions()
	var always := AlwaysCounter.new()
	always.name = "Always"
	always.process_mode = Node.PROCESS_MODE_ALWAYS
	add_child(always)
	var tick := Timer.new()
	tick.name = "Tick"
	tick.wait_time = 0.05
	tick.process_callback = Timer.TIMER_PROCESS_PHYSICS
	tick.autostart = true
	tick.timeout.connect(func(): timer_fires += 1)
	add_child(tick)
	_faller = RigidBody2D.new()
	_faller.name = "Faller"
	_faller.can_sleep = false
	_faller.linear_damp_mode = RigidBody2D.DAMP_MODE_REPLACE
	_faller.linear_damp = 0.0
	var shape := CollisionShape2D.new()
	shape.shape = CircleShape2D.new()
	_faller.add_child(shape)
	add_child(_faller)
	get_tree().physics_frame.connect(_on_physics_frame)
	_arm_scene_timer()
	_note("frames ready")


func _add_actions() -> void:
	if InputMap.has_action("jump"):
		return
	InputMap.add_action("jump")
	var key := InputEventKey.new()
	key.keycode = KEY_SPACE
	InputMap.action_add_event("jump", key)
	var button := InputEventJoypadButton.new()
	button.button_index = JOY_BUTTON_A
	InputMap.action_add_event("jump", button)


func _arm_scene_timer() -> void:
	get_tree().create_timer(0.05).timeout.connect(_on_scene_timer)


func _on_scene_timer() -> void:
	scene_timer_fires += 1
	if is_inside_tree():
		_arm_scene_timer()


func _on_physics_frame() -> void:
	signal_calls += 1
	last_signal_frame = Engine.get_physics_frames()


static func _keep(list: Array, value) -> void:
	list.append(value)
	if list.size() > MAX_RECORDED:
		list.pop_front()


func _note(text: String) -> void:
	print("runtime-control: ", text)
	var path := "user://markers.log"
	var file := FileAccess.open(path, FileAccess.READ_WRITE if FileAccess.file_exists(path) else FileAccess.WRITE)
	if file != null:
		file.seek_end()
		file.store_line(text)
		file.close()


func _notification(what: int) -> void:
	if what == NOTIFICATION_PAUSED:
		_note("tree paused")
	elif what == NOTIFICATION_UNPAUSED:
		_note("tree unpaused")


func _physics_process(_delta: float) -> void:
	physics_calls += 1
	var frame := Engine.get_physics_frames()
	last_physics_frame = frame
	_keep(physics_frames, frame)
	var pressed := Input.is_action_pressed("jump")
	if Input.is_action_just_pressed("jump"):
		_keep(just_pressed, frame)
	if Input.is_action_just_released("jump"):
		_keep(just_released, frame)
	if pressed and not _was_pressed:
		_keep(pressed_from, frame)
	elif _was_pressed and not pressed:
		_keep(released_from, frame)
	_was_pressed = pressed
	if pause_after_physics > 0:
		pause_after_physics -= 1
		if pause_after_physics == 0:
			paused = true


func _process(_delta: float) -> void:
	process_calls += 1
	if stall_ms > 0:
		OS.delay_msec(stall_ms)


func _input(event: InputEvent) -> void:
	if event.is_action("jump"):
		_keep(input_seen, [Engine.get_physics_frames(), Engine.is_in_physics_frame(), event.is_pressed()])


func _unhandled_input(event: InputEvent) -> void:
	if event.is_action("jump"):
		_keep(unhandled_seen, [Engine.get_physics_frames(), Engine.is_in_physics_frame(), event.is_pressed()])
