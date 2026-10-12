extends Control

## The caller side of the impact query. `_on_player_health_changed` is the
## `[connection]` handler declared by `scenes/main.tscn`, and `nudge` calls
## `take_damage` through a *qualified* receiver, which is the only form Todo 10
## offers cross-script candidates for — and it stays an `inferred` call edge, never
## exact, because GDScript dispatch depends on the runtime type.

@export var player: CorePlayer

func _on_player_health_changed(amount: int) -> void:
	nudge(amount)

func nudge(amount: int) -> void:
	player.take_damage(amount)
