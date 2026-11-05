extends Control

signal start_requested


func _ready() -> void:
	GameState.score_changed.connect(_on_score_changed)


func _on_button_pressed() -> void:
	start_requested.emit()


func _on_player_health_changed(value: int) -> void:
	$Label.text = str(value)


func _on_score_changed(_score: int) -> void:
	pass


func _on_enemy_left() -> void:
	pass
