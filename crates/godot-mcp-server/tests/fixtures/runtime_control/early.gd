extends Node
## An autoload at the runtime probe's own physics priority, the lowest there
## is. Autoloads come before the probe in the scene tree, so in every physics
## frame this one processes before the probe does. The frame-stepping cases
## read which physics frames it processed.

const MAX_RECORDED := 2000

var physics_calls := 0
## Engine.get_physics_frames() in each of its last MAX_RECORDED physics frames.
var physics_frames: Array[int] = []


func _ready() -> void:
	process_physics_priority = -2147483648


func _physics_process(_delta: float) -> void:
	physics_calls += 1
	physics_frames.append(Engine.get_physics_frames())
	if physics_frames.size() > MAX_RECORDED:
		physics_frames.pop_front()
