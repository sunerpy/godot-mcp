extends Node

## A deliberately dangling referrer. `res://scripts/absent.gd` does not exist in
## this fixture, so the dangling audit must report `target_absent` for it, and a
## computed `load()` path must surface the recorded limitation rather than a
## fabricated edge.

const MISSING := preload("res://scripts/absent.gd")

func pick(name: String) -> Resource:
	return load("res://scripts/%s.gd" % name)
