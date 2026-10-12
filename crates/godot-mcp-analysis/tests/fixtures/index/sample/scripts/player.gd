class_name IndexPlayer
extends Node

## Attached to `main.tscn::Player` and bound by `resources/buff.tres`. Editing
## this file must invalidate its own slice and the slices that reference it, and
## nothing else.

signal health_changed(amount: int)

const BUFF := preload("res://resources/buff.tres")

func take_damage(amount: int) -> void:
	health_changed.emit(amount)
	GameState.register_hit()
