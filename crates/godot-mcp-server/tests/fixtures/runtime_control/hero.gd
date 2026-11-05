class_name RuntimeHero
extends CharacterBody2D
## The fixture's hero: script properties of each shape a property write
## converts, and two whose setters change or ignore what is written.

var speed: float = 120.0
var health: int = 10
var title: String = "hero"
var tags: Array[String] = ["hero"]
var stock: Dictionary[String, int] = {}
var cell: Vector2i = Vector2i.ZERO
var anything = null
var clamped: int = 0:
	set(value):
		clamped = clampi(value, 0, 100)
var ignored: int = 7:
	set(_value):
		pass
