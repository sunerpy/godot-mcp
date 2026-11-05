extends RefCounted


static func clamp_health(value: int) -> int:
	return clampi(value, 0, 100)
