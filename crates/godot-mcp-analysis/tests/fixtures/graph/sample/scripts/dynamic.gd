extends Node

## Every dynamic form the graph must stay honest about.

func dispatch(target, path: String) -> void:
	target.take_damage(1)
	target.only_here()
	queue_redraw()
	var scene = load(path)
	set_process(scene != null)

func only_here() -> void:
	pass
