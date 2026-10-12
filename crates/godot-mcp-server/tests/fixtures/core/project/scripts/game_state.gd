extends Node

## The autoload singleton target. Registered as `GameState` by `project.godot`,
## so a project-dependency answer must show the autoload leg.

var hits: int = 0

func register_hit() -> void:
	hits += 1
