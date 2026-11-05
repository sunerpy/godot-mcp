extends RefCounted
## Value rules of the runtime probe, free of any scene: how a decoded tagged
## value becomes a property's declared type without losing anything, how a
## wait compares a live value with an expected one, what a sampled value may
## be and how a numeric series is summarized.
##
## Godot converts silently and lossily where these rules refuse: it truncates
## a float written to an `int` property and drops an untyped array written to a
## typed one without a word. A write the probe makes is therefore converted
## here first, exactly or not at all, and read back afterwards.

const SAFE_INTEGER := 9007199254740992.0
## The longest string a sample keeps.
const MAX_SAMPLED_STRING := 256
## Every comparison a wait evaluates.
const COMPARISONS := ["==", "!=", "<", "<=", ">", ">=", "contains", "approx"]

## The element type of each packed array type.
const PACKED_ELEMENT := {
	TYPE_PACKED_BYTE_ARRAY: TYPE_INT,
	TYPE_PACKED_INT32_ARRAY: TYPE_INT,
	TYPE_PACKED_INT64_ARRAY: TYPE_INT,
	TYPE_PACKED_FLOAT32_ARRAY: TYPE_FLOAT,
	TYPE_PACKED_FLOAT64_ARRAY: TYPE_FLOAT,
	TYPE_PACKED_STRING_ARRAY: TYPE_STRING,
	TYPE_PACKED_VECTOR2_ARRAY: TYPE_VECTOR2,
	TYPE_PACKED_VECTOR3_ARRAY: TYPE_VECTOR3,
	TYPE_PACKED_COLOR_ARRAY: TYPE_COLOR,
	TYPE_PACKED_VECTOR4_ARRAY: TYPE_VECTOR4,
}
## The whole-number limits of each packed integer array type.
const PACKED_RANGE := {
	TYPE_PACKED_BYTE_ARRAY: [0.0, 255.0],
	TYPE_PACKED_INT32_ARRAY: [-2147483648.0, 2147483647.0],
	TYPE_PACKED_INT64_ARRAY: [-SAFE_INTEGER, SAFE_INTEGER],
}
## Types a sample keeps: small, fixed-size values and short strings.
const SAMPLED := [
	TYPE_NIL, TYPE_BOOL, TYPE_INT, TYPE_FLOAT, TYPE_STRING, TYPE_STRING_NAME, TYPE_NODE_PATH,
	TYPE_VECTOR2, TYPE_VECTOR2I, TYPE_VECTOR3, TYPE_VECTOR3I, TYPE_VECTOR4, TYPE_VECTOR4I,
	TYPE_RECT2, TYPE_RECT2I, TYPE_COLOR, TYPE_QUATERNION, TYPE_PLANE, TYPE_AABB,
	TYPE_TRANSFORM2D, TYPE_BASIS, TYPE_TRANSFORM3D,
]
const VECTORS := [TYPE_VECTOR2, TYPE_VECTOR2I, TYPE_VECTOR3, TYPE_VECTOR3I, TYPE_VECTOR4, TYPE_VECTOR4I]
const STRINGS := [TYPE_STRING, TYPE_STRING_NAME, TYPE_NODE_PATH]


static func _good(value) -> Dictionary:
	return {"ok": true, "value": value}


static func _bad(reason: String) -> Dictionary:
	return {"ok": false, "reason": reason}


static func is_number(value) -> bool:
	return typeof(value) == TYPE_INT or typeof(value) == TYPE_FLOAT


static func _whole(number: float) -> bool:
	return is_finite(number) and number == floor(number) and absf(number) <= SAFE_INTEGER


static func _whole_components(value) -> bool:
	match typeof(value):
		TYPE_VECTOR2:
			return _whole(value.x) and _whole(value.y)
		TYPE_VECTOR3:
			return _whole(value.x) and _whole(value.y) and _whole(value.z)
		TYPE_VECTOR4:
			return _whole(value.x) and _whole(value.y) and _whole(value.z) and _whole(value.w)
		TYPE_RECT2:
			return _whole_components(value.position) and _whole_components(value.size)
	return false


# --- writes --------------------------------------------------------------------


## `value` as the property `info` (one entry of an object's property list)
## declares it, given the property's `current` value: {"ok": true, "value"} or
## {"ok": false, "reason"}. An untyped property takes any value.
static func convert_for(info: Dictionary, current, value) -> Dictionary:
	var type: int = int(info.get("type", TYPE_NIL))
	if type == TYPE_NIL:
		return _good(value)
	return convert_to(type, str(info.get("class_name", "")), current, value)


## `value` as a value of Variant type `type` (of class `class_hint`, for an
## object), converted only where nothing is lost.
static func convert_to(type: int, class_hint: String, current, value) -> Dictionary:
	var given: int = typeof(value)
	if type == TYPE_OBJECT:
		return _object(class_hint, value)
	if given == type:
		if type == TYPE_ARRAY:
			return _typed_array(current, value)
		if type == TYPE_DICTIONARY:
			return _typed_dictionary(current, value)
		return _good(value)
	match type:
		TYPE_FLOAT:
			if given == TYPE_INT:
				return _good(float(value))
		TYPE_INT:
			if given == TYPE_FLOAT and _whole(value):
				return _good(int(value))
			if given == TYPE_FLOAT:
				return _bad("an int property cannot hold %s without truncating it" % str(value))
		TYPE_STRING:
			if given == TYPE_STRING_NAME or given == TYPE_NODE_PATH:
				return _good(String(value))
		TYPE_STRING_NAME:
			if given == TYPE_STRING:
				return _good(StringName(value))
		TYPE_NODE_PATH:
			if given == TYPE_STRING:
				return _good(NodePath(value))
		TYPE_VECTOR2:
			if given == TYPE_VECTOR2I:
				return _good(Vector2(value))
		TYPE_VECTOR2I:
			if given == TYPE_VECTOR2 and _whole_components(value):
				return _good(Vector2i(value))
		TYPE_VECTOR3:
			if given == TYPE_VECTOR3I:
				return _good(Vector3(value))
		TYPE_VECTOR3I:
			if given == TYPE_VECTOR3 and _whole_components(value):
				return _good(Vector3i(value))
		TYPE_VECTOR4:
			if given == TYPE_VECTOR4I:
				return _good(Vector4(value))
		TYPE_VECTOR4I:
			if given == TYPE_VECTOR4 and _whole_components(value):
				return _good(Vector4i(value))
		TYPE_RECT2:
			if given == TYPE_RECT2I:
				return _good(Rect2(value))
		TYPE_RECT2I:
			if given == TYPE_RECT2 and _whole_components(value):
				return _good(Rect2i(value))
	if PACKED_ELEMENT.has(type) and given == TYPE_ARRAY:
		return _packed(type, value)
	return _bad("the property is %s; a %s value does not convert to it without loss" % [
		type_string(type), type_string(given)])


static func _packed(type: int, value: Array) -> Dictionary:
	var element: int = PACKED_ELEMENT[type]
	var items: Array = []
	for index in value.size():
		var converted: Dictionary = convert_to(element, "", null, value[index])
		if not converted["ok"]:
			return _bad("item %d: %s" % [index, converted["reason"]])
		if PACKED_RANGE.has(type):
			var limits: Array = PACKED_RANGE[type]
			var number := float(converted["value"])
			if number < limits[0] or number > limits[1]:
				return _bad("item %d is outside the range a %s holds" % [index, type_string(type)])
		items.append(converted["value"])
	return _good(type_convert(items, type))


static func _object(class_hint: String, value) -> Dictionary:
	if value == null:
		return _good(null)
	if typeof(value) != TYPE_OBJECT:
		return _bad("the property holds an object; a %s value is not one" % type_string(typeof(value)))
	if class_hint == "" or value.is_class(class_hint) or _script_class_is(value, class_hint):
		return _good(value)
	return _bad("the property holds a %s; %s is a %s" % [
		class_hint, str(value.resource_path) if value is Resource else "the value", value.get_class()])


## Whether `object`'s script, or a script it extends, is the global class
## `name`.
static func _script_class_is(object: Object, name: String) -> bool:
	var script = object.get_script()
	while script != null:
		if str(script.get_global_name()) == name:
			return true
		script = script.get_base_script()
	return false


static func _typed_array(current, value: Array) -> Dictionary:
	if typeof(current) != TYPE_ARRAY or not current.is_typed():
		return _good(value)
	var builtin: int = current.get_typed_builtin()
	var class_hint := String(current.get_typed_class_name())
	var items: Array = []
	for index in value.size():
		var converted: Dictionary = convert_to(builtin, class_hint, null, value[index])
		if not converted["ok"]:
			return _bad("item %d: %s" % [index, converted["reason"]])
		items.append(converted["value"])
	return _good(Array(items, builtin, current.get_typed_class_name(), current.get_typed_script()))


static func _typed_dictionary(current, value: Dictionary) -> Dictionary:
	if typeof(current) != TYPE_DICTIONARY or not current.is_typed():
		return _good(value)
	var key_type: int = current.get_typed_key_builtin()
	var value_type: int = current.get_typed_value_builtin()
	var entries := {}
	for key in value.keys():
		var converted_key: Dictionary = {"ok": true, "value": key}
		if current.is_typed_key():
			converted_key = convert_to(key_type, String(current.get_typed_key_class_name()), null, key)
			if not converted_key["ok"]:
				return _bad("key %s: %s (keys arrive as strings)" % [str(key), converted_key["reason"]])
		var converted_value: Dictionary = {"ok": true, "value": value[key]}
		if current.is_typed_value():
			converted_value = convert_to(value_type, String(current.get_typed_value_class_name()), null, value[key])
			if not converted_value["ok"]:
				return _bad("value of %s: %s" % [str(key), converted_value["reason"]])
		entries[converted_key["value"]] = converted_value["value"]
	return _good(Dictionary(entries,
		key_type, current.get_typed_key_class_name(), current.get_typed_key_script(),
		value_type, current.get_typed_value_class_name(), current.get_typed_value_script()))


## Whether `a` and `b` are the same value: of one type (numbers of either
## kind compare by value) and equal.
static func same(a, b) -> bool:
	if is_number(a) and is_number(b):
		return float(a) == float(b)
	if typeof(a) != typeof(b):
		return false
	return a == b


# --- comparisons -----------------------------------------------------------------


## Whether live `actual` stands in relation `op` to `expected`:
## {"ok": true, "value": bool}, or {"ok": false, "reason"} when the comparison
## does not apply to these values, which waiting cannot change.
static func compare(actual, op: String, expected, tolerance: float) -> Dictionary:
	if not COMPARISONS.has(op):
		return _bad("%s is not a comparison" % op)
	var a: int = typeof(actual)
	var e: int = typeof(expected)
	if op == "contains":
		return _contains(actual, expected)
	if a == TYPE_NIL or e == TYPE_NIL:
		if op == "==":
			return _good(a == e)
		if op == "!=":
			return _good(a != e)
		return _bad("only == and != compare with null")
	if is_number(actual) and is_number(expected):
		var x := float(actual)
		var y := float(expected)
		match op:
			"==":
				return _good(x == y)
			"!=":
				return _good(x != y)
			"<":
				return _good(x < y)
			"<=":
				return _good(x <= y)
			">":
				return _good(x > y)
			">=":
				return _good(x >= y)
			"approx":
				return _good(absf(x - y) <= tolerance)
	if STRINGS.has(a) and STRINGS.has(e):
		match op:
			"==":
				return _good(String(actual) == String(expected))
			"!=":
				return _good(String(actual) != String(expected))
		return _bad("strings compare with ==, != and contains")
	if a != e and not (VECTORS.has(a) and VECTORS.has(e) and _dimension(a) == _dimension(e)):
		return _bad("the live value is %s; it never compares with a %s" % [type_string(a), type_string(e)])
	match op:
		"==":
			return _good(_vector(actual) == _vector(expected) if VECTORS.has(a) else actual == expected)
		"!=":
			return _good(_vector(actual) != _vector(expected) if VECTORS.has(a) else actual != expected)
		"approx":
			if VECTORS.has(a):
				return _good(_vector(actual).distance_to(_vector(expected)) <= tolerance)
			if a == TYPE_COLOR or a == TYPE_QUATERNION:
				var worst := 0.0
				for component in 4:
					var difference := absf(float(actual[component]) - float(expected[component]))
					# maxf keeps the later of its arguments when either is NaN,
					# so a NaN component is decided here, never lost.
					if is_nan(difference):
						return _good(false)
					worst = maxf(worst, difference)
				return _good(worst <= tolerance)
			return _bad("approx compares numbers, vectors, colors and quaternions")
	return _bad("%s does not order %s values" % [op, type_string(a)])


static func _dimension(type: int) -> int:
	match type:
		TYPE_VECTOR2, TYPE_VECTOR2I:
			return 2
		TYPE_VECTOR3, TYPE_VECTOR3I:
			return 3
	return 4


## A vector as its floating-point kind, so whole and real vectors compare.
static func _vector(value):
	match typeof(value):
		TYPE_VECTOR2I:
			return Vector2(value)
		TYPE_VECTOR3I:
			return Vector3(value)
		TYPE_VECTOR4I:
			return Vector4(value)
	return value


static func _contains(actual, expected) -> Dictionary:
	var a: int = typeof(actual)
	if STRINGS.has(a):
		if not STRINGS.has(typeof(expected)):
			return _bad("a string contains only strings")
		return _good(String(actual).contains(String(expected)))
	if a == TYPE_DICTIONARY:
		return _good(actual.has(expected))
	if a == TYPE_ARRAY or PACKED_ELEMENT.has(a):
		for item in actual:
			if same(item, expected):
				return _good(true)
		return _good(false)
	return _bad("contains applies to strings, arrays and dictionaries, not %s" % type_string(a))


# --- samples ---------------------------------------------------------------------


## Why `value` is not kept in a sample, or "" when it is.
static func unsampled(value) -> String:
	if not SAMPLED.has(typeof(value)):
		return "a %s is not sampled; read it with runtime.properties.get" % type_string(typeof(value))
	if STRINGS.has(typeof(value)) and String(value).length() > MAX_SAMPLED_STRING:
		return "a string longer than %d characters is not sampled" % MAX_SAMPLED_STRING
	return ""


## The minimum, maximum, mean and last of the numbers in `values`, which may
## hold nulls; null when none is a number.
static func stats(values: Array):
	var count := 0
	var total := 0.0
	var low := INF
	var high := -INF
	var last = null
	for value in values:
		if not is_number(value):
			continue
		var number := float(value)
		count += 1
		total += number
		low = minf(low, number)
		high = maxf(high, number)
		last = value
	if count == 0:
		return null
	return {"count": count, "min": low, "max": high, "mean": total / count, "last": last}
