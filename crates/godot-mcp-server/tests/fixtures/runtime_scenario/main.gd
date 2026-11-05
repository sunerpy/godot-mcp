extends Node2D
## The runtime-scenario fixture: a small game the live scenario suite drives,
## compares with reference images and stresses. Everything is built here, so
## the scene file is one root node.
##
## * `Swatch`, a 100x50 ColorRect at (80, 60) in a 320x240 window, clear of
##   the HUD, is what the image comparisons recolour;
## * `Player` stands at (10, 20);
## * the HUD holds the `Start` button, inside a headless session's 64x64
##   viewport, and the `Score` label that counts its presses;
## * `game_time` adds up the delta of every process frame, the game time the
##   engine's time scale speeds up and slows down, and `real_ms` is the
##   engine's tick clock read in the same frame;
## * `drift`, `heading` and `tint` hold values that are not finite.

var game_time := 0.0
var real_ms := 0
var score := 0
var drift := NAN
var heading := Vector2(NAN, 1.0)
var tint := Color(1.0, NAN, 0.0, 1.0)


func _ready() -> void:
	var swatch := ColorRect.new()
	swatch.name = "Swatch"
	swatch.position = Vector2(80, 60)
	swatch.size = Vector2(100, 50)
	swatch.color = Color(0.2, 0.4, 0.8, 1.0)
	swatch.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(swatch)
	var player := Node2D.new()
	player.name = "Player"
	player.position = Vector2(10, 20)
	add_child(player)
	var hud := CanvasLayer.new()
	hud.name = "HUD"
	add_child(hud)
	var start := Button.new()
	start.name = "Start"
	start.text = "Start"
	start.position = Vector2(2, 2)
	start.focus_mode = Control.FOCUS_NONE
	start.pressed.connect(_on_start)
	hud.add_child(start)
	var label := Label.new()
	label.name = "Score"
	label.text = "Score: 0"
	label.position = Vector2(200, 8)
	hud.add_child(label)


func _process(delta: float) -> void:
	game_time += delta
	real_ms = Time.get_ticks_msec()


func _on_start() -> void:
	score += 1
	($HUD/Score as Label).text = "Score: %d" % score
