extends Node

## Referenced by nothing. Present so the deletion path can remove a file whose
## disappearance cannot be masked by another document still naming it.

func spin(target) -> void:
	target.take_damage(1)
