extends CharacterBody2D
## The player: walks left and right with the arrow keys, jumps with Enter or
## Space, and stops at the walls and at the crate.

## How fast the player walks, in pixels per second.
@export var speed := 260.0
## How fast the player leaves the ground when it jumps, in pixels per second.
@export var jump_speed := 520.0

var _gravity: float = ProjectSettings.get_setting("physics/2d/default_gravity")

@onready var _sprite: Sprite2D = $Sprite


func _physics_process(delta: float) -> void:
	if is_on_floor():
		if Input.is_action_just_pressed("ui_accept"):
			velocity.y = -jump_speed
	else:
		velocity.y += _gravity * delta
	velocity.x = Input.get_axis("ui_left", "ui_right") * speed
	if velocity.x != 0.0:
		_sprite.flip_h = velocity.x < 0.0
	move_and_slide()
