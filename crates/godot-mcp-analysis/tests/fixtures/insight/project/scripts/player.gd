class_name Player
extends CharacterBody2D

signal health_changed(value: int)
signal died

@export var max_health: int = 100
@export_range(0.0, 600.0) var speed := 300.0
@export_group("Combat")
@export var damage: int = 10

var health := max_health


func take_hit(attacker: Enemy, amount: int) -> void:
	if amount <= 0 or attacker == null:
		return
	health -= amount
	health_changed.emit(health)
	if health <= 0:
		died.emit()
	elif health < max_health / 4 and attacker.is_elite:
		for i in range(3):
			while i > 0:
				i -= 1
	else:
		match attacker.kind:
			"melee":
				pass
			_:
				pass


func _physics_process(_delta: float) -> void:
	move_and_slide()
