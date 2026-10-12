class_name HealthyPlayer
extends CharacterBody2D

const MAX_HEALTH: int = 100
const BULLET := preload("res://scenes/bullet.tscn")

signal health_changed(new_health: int)
signal died

enum State { IDLE, RUNNING, DEAD }

enum Team {
	RED = 0,
	BLUE = 1,
}

@export var speed: float = 240.0
var health: int = MAX_HEALTH
var _state: State = State.IDLE

class Inventory:
	var slots: int = 8

	func capacity() -> int:
		return slots

	static func default_capacity() -> int:
		return 8

func _init() -> void:
	health = MAX_HEALTH

func _ready() -> void:
	var manager := get_node("/root/GameManager")
	manager.register(self)
	health_changed.emit(health)

func take_damage(amount: int, source: Node = null) -> int:
	health = max(0, health - amount)
	health_changed.emit(health)
	if health == 0:
		die()
	return health

func die() -> void:
	_state = State.DEAD
	died.emit()

static func clamp_health(value: int) -> int:
	return clampi(value, 0, MAX_HEALTH)
