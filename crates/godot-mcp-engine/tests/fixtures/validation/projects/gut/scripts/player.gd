extends CharacterBody2D

const SPEED := 220.0

func _physics_process(delta: float) -> void:
	velocity.x = SPEED * delta
	move_and_slide()
