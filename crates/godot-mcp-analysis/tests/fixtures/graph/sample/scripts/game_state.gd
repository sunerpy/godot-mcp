extends Node

## Autoload singleton target referenced from `[autoload] GameState`.

var hits: int = 0

func register_hit() -> void:
	hits += 1

func take_damage(amount: int) -> void:
	hits += amount
