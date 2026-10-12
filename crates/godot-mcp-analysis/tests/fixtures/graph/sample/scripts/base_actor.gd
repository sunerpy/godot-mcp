class_name BaseActor
extends Node

## The base of the fixture's script hierarchy. `extends Node` names an engine
## class, which this layer deliberately cannot confirm.

func take_damage(amount: int) -> void:
	set_process(amount > 0)

func shared_helper() -> void:
	pass
