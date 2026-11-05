extends Node

signal score_changed(score: int)

const PLAYER_SCENE := preload("res://scenes/player.tscn")

var score := 0


func add_points(points: int) -> void:
	score += points
	score_changed.emit(score)
