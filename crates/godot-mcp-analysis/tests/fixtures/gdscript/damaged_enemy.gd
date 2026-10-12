class_name DamagedEnemy
extends Node2D

signal alerted

func attack(target: Node) -> int:
	return deal_damage(target)

static func spawn_cost() -> int:
	return 7

func patrol() -> void:
	move_along_path(
