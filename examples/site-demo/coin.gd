extends Area2D
## A coin. When the player touches it, it tells the game and disappears.

signal collected


func _on_body_entered(body: Node2D) -> void:
	if not body.is_in_group("player"):
		return
	set_deferred("monitoring", false)
	collected.emit()
	queue_free()
