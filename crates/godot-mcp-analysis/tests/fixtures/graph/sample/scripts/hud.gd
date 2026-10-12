extends CanvasLayer

func _on_player_health_changed(amount: int) -> void:
	set_process(amount > 0)
