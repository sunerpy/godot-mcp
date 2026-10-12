extends Node

## Autoload singleton, registered as `[autoload] GameState`. The
## index-persistence tests edit this file to prove that an autoload edit
## invalidates exactly its own slice plus the project slice that names it.

var hits: int = 0

func register_hit() -> void:
	hits += 1
