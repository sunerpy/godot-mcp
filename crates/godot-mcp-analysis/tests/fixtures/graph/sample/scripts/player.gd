extends BaseActor

signal health_changed(amount: int)

const BUFF := preload("res://resources/buff.tres")

func take_damage(amount: int) -> void:
	health_changed.emit(amount)
	GameState.register_hit()
	shared_helper()
