extends CharacterBody2D

signal health_changed(amount: int)

func _on_overlay_pressed() -> void:
	health_changed.emit(1)
