extends CanvasLayer

## Signal handler reached by `main.tscn`'s `[connection]`. Untouched by the
## incremental tests: its slice hash must survive an edit to another file.

func _on_player_health_changed(amount: int) -> void:
	set_process(amount > 0)
