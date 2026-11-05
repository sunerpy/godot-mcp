class_name Enemy
extends CharacterBody2D

@export var speed := 120.0
@export var title: String = "grunt"

var is_elite := false
var kind := "melee"


func attack(target: Player) -> void:
	target.take_hit(self, 5)
