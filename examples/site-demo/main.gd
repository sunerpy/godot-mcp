extends Node2D
## The demo game. The player walks with the arrow keys and jumps with Enter or
## Space; each coin it touches adds one to `score`, which the HUD shows as a
## number.

## How many coins the player has collected.
var score := 0

@onready var _count: Label = $HUD/Count


func _ready() -> void:
	_count.text = str(score)


func _on_coin_collected() -> void:
	score += 1
	_count.text = str(score)
