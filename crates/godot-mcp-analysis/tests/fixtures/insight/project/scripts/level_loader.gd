extends Node2D

@export var first_level := "level_1"


func _ready() -> void:
	var level := load("res://levels/" + first_level + ".tscn")
	add_child(level.instantiate())
	for file in DirAccess.get_files_at("res://levels"):
		print(file)
