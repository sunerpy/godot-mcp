class_name CorePlayer
extends Node

## The impact subject of the Todo 12 happy path. It is attached to
## `scenes/main.tscn::Player`, bound by `resources/buff.tres`, called from
## `scripts/hud.gd`, and preloads a text resource, so a bounded impact query over
## it must reach a scene, a scene node, a text resource and a caller script.

signal health_changed(amount: int)

const BUFF := preload("res://resources/buff.tres")

func take_damage(amount: int) -> void:
	health_changed.emit(amount)
	GameState.register_hit()

func heal(amount: int) -> void:
	health_changed.emit(-amount)
