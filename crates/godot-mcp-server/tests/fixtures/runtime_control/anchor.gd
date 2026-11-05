extends Node2D
## Keeps its node where it started, every physics frame: a stand-in for game
## code that holds a node in place, so a direct move makes no progress.

var home := Vector2.ZERO


func _ready() -> void:
	home = global_position


func _physics_process(_delta: float) -> void:
	global_position = home
