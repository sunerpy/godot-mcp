@tool
extends RefCounted
## The contract's tagged value spelling, `{"type": ..., "value": ...}`, to and
## from Godot Variants.
##
## The spelling is the server's `GodotValue`: snake_case tags (`vector2`,
## `transform2_d`, `packed_int32_array`, ...) and named fields (`x`/`y`, `red`/
## `green`/`blue`/`alpha`, `position`/`size`, `basis`/`origin`). A `projection`
## is four `rows`, where row r holds component r of the columns x, y, z and w.
##
## Both directions are bounded: nesting depth, collection length and string
## length. Decoding refuses what the contract cannot carry exactly — an integer
## beyond 2^53 (JSON numbers arrive as doubles), a non-finite float, a malformed
## field — and anything the editor must not receive: an object reference. A
## `resource` is resolved by loading its `res://` path and checking its class.
## Encoding never serializes object internals: a resource saved to its own file
## becomes a `resource` reference and every other object is refused.

const MAX_DEPTH := 16
const MAX_ITEMS := 4096
const MAX_STRING := 65536
const SAFE_INTEGER := 9007199254740992.0


static func _good(value) -> Dictionary:
	return {"ok": true, "value": value}


static func _bad(reason: String) -> Dictionary:
	return {"ok": false, "reason": reason}


static func _xy(value) -> Dictionary:
	return {"x": value.x, "y": value.y}


static func _xyz(value) -> Dictionary:
	return {"x": value.x, "y": value.y, "z": value.z}


static func _xyzw(value) -> Dictionary:
	return {"x": value.x, "y": value.y, "z": value.z, "w": value.w}


static func _rgba(value: Color) -> Dictionary:
	return {"red": value.r, "green": value.g, "blue": value.b, "alpha": value.a}


static func _finite(value: float) -> bool:
	return not is_nan(value) and not is_inf(value)


## Encode `value` as a tagged value. Returns {"ok": true, "value": tagged} or
## {"ok": false, "reason": why}.
static func encode(value, depth: int = 0) -> Dictionary:
	if depth > MAX_DEPTH:
		return _bad("the value nests deeper than %d levels" % MAX_DEPTH)
	match typeof(value):
		TYPE_NIL:
			return _good({"type": "null"})
		TYPE_BOOL:
			return _good({"type": "bool", "value": value})
		TYPE_INT:
			return _good({"type": "integer", "value": value})
		TYPE_FLOAT:
			if not _finite(value):
				return _bad("a non-finite float has no JSON spelling")
			return _good({"type": "float", "value": value})
		TYPE_STRING:
			if value.length() > MAX_STRING:
				return _bad("the string is longer than %d characters" % MAX_STRING)
			return _good({"type": "string", "value": value})
		TYPE_STRING_NAME:
			return _good({"type": "string_name", "value": String(value)})
		TYPE_NODE_PATH:
			return _good({"type": "node_path", "value": String(value)})
		TYPE_VECTOR2:
			return _good({"type": "vector2", "value": _xy(value)})
		TYPE_VECTOR2I:
			return _good({"type": "vector2i", "value": _xy(value)})
		TYPE_RECT2:
			return _good({"type": "rect2", "value": {"position": _xy(value.position), "size": _xy(value.size)}})
		TYPE_RECT2I:
			return _good({"type": "rect2i", "value": {"position": _xy(value.position), "size": _xy(value.size)}})
		TYPE_VECTOR3:
			return _good({"type": "vector3", "value": _xyz(value)})
		TYPE_VECTOR3I:
			return _good({"type": "vector3i", "value": _xyz(value)})
		TYPE_TRANSFORM2D:
			return _good({"type": "transform2_d", "value": {
				"x": _xy(value.x), "y": _xy(value.y), "origin": _xy(value.origin)}})
		TYPE_VECTOR4:
			return _good({"type": "vector4", "value": _xyzw(value)})
		TYPE_VECTOR4I:
			return _good({"type": "vector4i", "value": _xyzw(value)})
		TYPE_PLANE:
			return _good({"type": "plane", "value": {"normal": _xyz(value.normal), "distance": value.d}})
		TYPE_QUATERNION:
			return _good({"type": "quaternion", "value": _xyzw(value)})
		TYPE_AABB:
			return _good({"type": "aabb", "value": {"position": _xyz(value.position), "size": _xyz(value.size)}})
		TYPE_BASIS:
			return _good({"type": "basis", "value": {"x": _xyz(value.x), "y": _xyz(value.y), "z": _xyz(value.z)}})
		TYPE_TRANSFORM3D:
			var basis: Basis = value.basis
			return _good({"type": "transform3_d", "value": {
				"basis": {"x": _xyz(basis.x), "y": _xyz(basis.y), "z": _xyz(basis.z)},
				"origin": _xyz(value.origin)}})
		TYPE_PROJECTION:
			var rows: Array = []
			for row in 4:
				rows.append([value.x[row], value.y[row], value.z[row], value.w[row]])
			return _good({"type": "projection", "value": {"matrix": {"rows": rows}}})
		TYPE_COLOR:
			return _good({"type": "color", "value": _rgba(value)})
		TYPE_ARRAY:
			if value.size() > MAX_ITEMS:
				return _bad("the array holds more than %d items" % MAX_ITEMS)
			var items: Array = []
			for item in value:
				var encoded: Dictionary = encode(item, depth + 1)
				if not encoded["ok"]:
					return encoded
				items.append(encoded["value"])
			return _good({"type": "array", "value": items})
		TYPE_DICTIONARY:
			if value.size() > MAX_ITEMS:
				return _bad("the dictionary holds more than %d entries" % MAX_ITEMS)
			var entries: Dictionary = {}
			for key in value.keys():
				if typeof(key) != TYPE_STRING and typeof(key) != TYPE_STRING_NAME:
					return _bad("only string dictionary keys have a tagged spelling")
				var encoded: Dictionary = encode(value[key], depth + 1)
				if not encoded["ok"]:
					return encoded
				entries[String(key)] = encoded["value"]
			return _good({"type": "dictionary", "value": entries})
		TYPE_PACKED_BYTE_ARRAY:
			return _packed("packed_byte_array", value)
		TYPE_PACKED_INT32_ARRAY:
			return _packed("packed_int32_array", value)
		TYPE_PACKED_INT64_ARRAY:
			return _packed("packed_int64_array", value)
		TYPE_PACKED_FLOAT32_ARRAY:
			return _packed("packed_float32_array", value)
		TYPE_PACKED_FLOAT64_ARRAY:
			return _packed("packed_float64_array", value)
		TYPE_PACKED_STRING_ARRAY:
			return _packed("packed_string_array", value)
		TYPE_PACKED_VECTOR2_ARRAY:
			if value.size() > MAX_ITEMS:
				return _bad("the array holds more than %d items" % MAX_ITEMS)
			var points: Array = []
			for point in value:
				points.append(_xy(point))
			return _good({"type": "packed_vector2_array", "value": points})
		TYPE_PACKED_VECTOR3_ARRAY:
			if value.size() > MAX_ITEMS:
				return _bad("the array holds more than %d items" % MAX_ITEMS)
			var points: Array = []
			for point in value:
				points.append(_xyz(point))
			return _good({"type": "packed_vector3_array", "value": points})
		TYPE_PACKED_COLOR_ARRAY:
			if value.size() > MAX_ITEMS:
				return _bad("the array holds more than %d items" % MAX_ITEMS)
			var colors: Array = []
			for color in value:
				colors.append(_rgba(color))
			return _good({"type": "packed_color_array", "value": colors})
		TYPE_OBJECT:
			return _encode_object(value)
		_:
			return _bad("values of Variant type %s have no tagged spelling" % type_string(typeof(value)))


static func _packed(tag: String, value) -> Dictionary:
	if value.size() > MAX_ITEMS:
		return _bad("the array holds more than %d items" % MAX_ITEMS)
	var items: Array = []
	for item in value:
		if typeof(item) == TYPE_FLOAT and not _finite(item):
			return _bad("a non-finite float has no JSON spelling")
		items.append(item)
	return _good({"type": tag, "value": items})


static func _encode_object(value) -> Dictionary:
	if value == null:
		return _good({"type": "null"})
	if value is Resource:
		var path: String = value.resource_path
		if path.begins_with("res://") and not path.contains("::"):
			var uid = null
			var id: int = ResourceLoader.get_resource_uid(path)
			if id != ResourceUID.INVALID_ID:
				uid = ResourceUID.id_to_text(id)
			return _good({"type": "resource", "value": {
				"resource": path, "resource_type": value.get_class(), "uid": uid}})
	return _bad("a %s object has no file of its own and is not serialized" % value.get_class())


static func _number(value, key: String) -> Dictionary:
	if typeof(value) != TYPE_DICTIONARY or not value.has(key):
		return _bad("missing numeric field `%s`" % key)
	var number = value[key]
	if typeof(number) != TYPE_FLOAT and typeof(number) != TYPE_INT:
		return _bad("field `%s` must be a number" % key)
	if not _finite(float(number)):
		return _bad("field `%s` must be finite" % key)
	return _good(float(number))


static func _integer(value, key: String) -> Dictionary:
	var number: Dictionary = _number(value, key)
	if not number["ok"]:
		return number
	var whole: float = number["value"]
	if whole != floor(whole) or absf(whole) > SAFE_INTEGER:
		return _bad("field `%s` must be a whole number within 2^53" % key)
	return _good(int(whole))


## Read the named numeric `fields` of `value`, as floats or (`whole`) ints.
static func _fields(value, fields: Array, whole: bool = false) -> Dictionary:
	var numbers: Array = []
	for field in fields:
		var read: Dictionary = _integer(value, field) if whole else _number(value, field)
		if not read["ok"]:
			return read
		numbers.append(read["value"])
	return _good(numbers)


static func _vec2(value, whole: bool = false) -> Dictionary:
	var read: Dictionary = _fields(value, ["x", "y"], whole)
	if not read["ok"]:
		return read
	var n: Array = read["value"]
	return _good(Vector2i(n[0], n[1]) if whole else Vector2(n[0], n[1]))


static func _vec3(value, whole: bool = false) -> Dictionary:
	var read: Dictionary = _fields(value, ["x", "y", "z"], whole)
	if not read["ok"]:
		return read
	var n: Array = read["value"]
	return _good(Vector3i(n[0], n[1], n[2]) if whole else Vector3(n[0], n[1], n[2]))


static func _vec4(value, whole: bool = false) -> Dictionary:
	var read: Dictionary = _fields(value, ["x", "y", "z", "w"], whole)
	if not read["ok"]:
		return read
	var n: Array = read["value"]
	return _good(Vector4i(n[0], n[1], n[2], n[3]) if whole else Vector4(n[0], n[1], n[2], n[3]))


static func _color(value) -> Dictionary:
	var read: Dictionary = _fields(value, ["red", "green", "blue", "alpha"])
	if not read["ok"]:
		return read
	var n: Array = read["value"]
	return _good(Color(n[0], n[1], n[2], n[3]))


static func _member(value, key: String):
	return value.get(key) if typeof(value) == TYPE_DICTIONARY else null


## Decode one tagged value. Returns {"ok": true, "value": variant} or
## {"ok": false, "reason": why}.
static func decode(tagged, depth: int = 0) -> Dictionary:
	if depth > MAX_DEPTH:
		return _bad("the value nests deeper than %d levels" % MAX_DEPTH)
	if typeof(tagged) != TYPE_DICTIONARY or not tagged.has("type"):
		return _bad("a value must be a tagged {type, value} object")
	var kind: String = str(tagged["type"])
	var value = tagged.get("value")
	match kind:
		"null":
			return _good(null)
		"bool":
			if typeof(value) != TYPE_BOOL:
				return _bad("a bool value must be true or false")
			return _good(value)
		"integer":
			return _integer({"value": value}, "value")
		"float":
			return _number({"value": value}, "value")
		"string", "string_name", "node_path":
			if typeof(value) != TYPE_STRING or value.length() > MAX_STRING:
				return _bad("a %s value must be a string of at most %d characters" % [kind, MAX_STRING])
			if kind == "string_name":
				return _good(StringName(value))
			if kind == "node_path":
				return _good(NodePath(value))
			return _good(value)
		"vector2":
			return _vec2(value)
		"vector2i":
			return _vec2(value, true)
		"vector3":
			return _vec3(value)
		"vector3i":
			return _vec3(value, true)
		"vector4":
			return _vec4(value)
		"vector4i":
			return _vec4(value, true)
		"rect2", "rect2i":
			var whole: bool = kind == "rect2i"
			var position: Dictionary = _vec2(_member(value, "position"), whole)
			var size: Dictionary = _vec2(_member(value, "size"), whole)
			if not position["ok"]:
				return position
			if not size["ok"]:
				return size
			if whole:
				return _good(Rect2i(position["value"], size["value"]))
			return _good(Rect2(position["value"], size["value"]))
		"transform2_d":
			var parts: Array = []
			for key in ["x", "y", "origin"]:
				var part: Dictionary = _vec2(_member(value, key))
				if not part["ok"]:
					return part
				parts.append(part["value"])
			return _good(Transform2D(parts[0], parts[1], parts[2]))
		"plane":
			var normal: Dictionary = _vec3(_member(value, "normal"))
			var distance: Dictionary = _number(value, "distance")
			if not normal["ok"]:
				return normal
			if not distance["ok"]:
				return distance
			return _good(Plane(normal["value"], distance["value"]))
		"quaternion":
			var q: Dictionary = _fields(value, ["x", "y", "z", "w"])
			if not q["ok"]:
				return q
			return _good(Quaternion(q["value"][0], q["value"][1], q["value"][2], q["value"][3]))
		"aabb":
			var position: Dictionary = _vec3(_member(value, "position"))
			var size: Dictionary = _vec3(_member(value, "size"))
			if not position["ok"]:
				return position
			if not size["ok"]:
				return size
			return _good(AABB(position["value"], size["value"]))
		"basis":
			return _basis(value)
		"transform3_d":
			var basis: Dictionary = _basis(_member(value, "basis"))
			var origin: Dictionary = _vec3(_member(value, "origin"))
			if not basis["ok"]:
				return basis
			if not origin["ok"]:
				return origin
			return _good(Transform3D(basis["value"], origin["value"]))
		"projection":
			var rows = _member(_member(value, "matrix"), "rows")
			if typeof(rows) != TYPE_ARRAY or rows.size() != 4:
				return _bad("a projection carries four rows")
			var columns: Array = [Vector4(), Vector4(), Vector4(), Vector4()]
			for r in 4:
				if typeof(rows[r]) != TYPE_ARRAY or rows[r].size() != 4:
					return _bad("every projection row carries four numbers")
				for c in 4:
					var cell = rows[r][c]
					if (typeof(cell) != TYPE_FLOAT and typeof(cell) != TYPE_INT) or not _finite(float(cell)):
						return _bad("projection cells must be finite numbers")
					var column: Vector4 = columns[c]
					column[r] = float(cell)
					columns[c] = column
			return _good(Projection(columns[0], columns[1], columns[2], columns[3]))
		"color":
			return _color(value)
		"array":
			if typeof(value) != TYPE_ARRAY or value.size() > MAX_ITEMS:
				return _bad("an array value must be a JSON array of at most %d items" % MAX_ITEMS)
			var items: Array = []
			for item in value:
				var decoded: Dictionary = decode(item, depth + 1)
				if not decoded["ok"]:
					return decoded
				items.append(decoded["value"])
			return _good(items)
		"dictionary":
			if typeof(value) != TYPE_DICTIONARY or value.size() > MAX_ITEMS:
				return _bad("a dictionary value must be a JSON object of at most %d entries" % MAX_ITEMS)
			var entries: Dictionary = {}
			for key in value.keys():
				var decoded: Dictionary = decode(value[key], depth + 1)
				if not decoded["ok"]:
					return decoded
				entries[key] = decoded["value"]
			return _good(entries)
		"packed_byte_array", "packed_int32_array", "packed_int64_array":
			if typeof(value) != TYPE_ARRAY or value.size() > MAX_ITEMS:
				return _bad("a %s must be a JSON array of at most %d items" % [kind, MAX_ITEMS])
			var limits := {"packed_byte_array": [0.0, 255.0], "packed_int32_array": [-2147483648.0, 2147483647.0],
				"packed_int64_array": [-SAFE_INTEGER, SAFE_INTEGER]}
			var low: float = limits[kind][0]
			var high: float = limits[kind][1]
			var whole_items: Array = []
			for item in value:
				var read: Dictionary = _integer({"v": item}, "v")
				if not read["ok"] or float(read["value"]) < low or float(read["value"]) > high:
					return _bad("a %s holds whole numbers in [%d, %d]" % [kind, low, high])
				whole_items.append(read["value"])
			if kind == "packed_byte_array":
				return _good(PackedByteArray(whole_items))
			if kind == "packed_int32_array":
				return _good(PackedInt32Array(whole_items))
			return _good(PackedInt64Array(whole_items))
		"packed_float32_array", "packed_float64_array":
			if typeof(value) != TYPE_ARRAY or value.size() > MAX_ITEMS:
				return _bad("a %s must be a JSON array of at most %d items" % [kind, MAX_ITEMS])
			var numbers: Array = []
			for item in value:
				var read: Dictionary = _number({"v": item}, "v")
				if not read["ok"]:
					return read
				numbers.append(read["value"])
			if kind == "packed_float32_array":
				return _good(PackedFloat32Array(numbers))
			return _good(PackedFloat64Array(numbers))
		"packed_string_array":
			if typeof(value) != TYPE_ARRAY or value.size() > MAX_ITEMS:
				return _bad("a packed_string_array must be a JSON array of at most %d strings" % MAX_ITEMS)
			var strings := PackedStringArray()
			for item in value:
				if typeof(item) != TYPE_STRING:
					return _bad("a packed_string_array holds strings")
				strings.append(item)
			return _good(strings)
		"packed_vector2_array", "packed_vector3_array", "packed_color_array":
			if typeof(value) != TYPE_ARRAY or value.size() > MAX_ITEMS:
				return _bad("a %s must be a JSON array of at most %d items" % [kind, MAX_ITEMS])
			var packed
			if kind == "packed_vector2_array":
				packed = PackedVector2Array()
			elif kind == "packed_vector3_array":
				packed = PackedVector3Array()
			else:
				packed = PackedColorArray()
			for item in value:
				var read: Dictionary
				if kind == "packed_vector2_array":
					read = _vec2(item)
				elif kind == "packed_vector3_array":
					read = _vec3(item)
				else:
					read = _color(item)
				if not read["ok"]:
					return read
				packed.append(read["value"])
			return _good(packed)
		"resource":
			var path: String = str(_member(value, "resource")) if _member(value, "resource") != null else ""
			if not path.begins_with("res://") or path.contains(".."):
				return _bad("a resource value names a res:// resource")
			var expected: String = str(_member(value, "resource_type")) if _member(value, "resource_type") != null else "Resource"
			if not ResourceLoader.exists(path):
				return _bad("the resource " + path + " does not exist")
			var resource: Resource = ResourceLoader.load(path)
			if resource == null:
				return _bad("the resource " + path + " cannot be loaded")
			if expected != "" and not resource.is_class(expected):
				return _bad(path + " is a " + resource.get_class() + ", not a " + expected)
			return _good(resource)
		"object":
			return _bad("a live object reference cannot be written; name a resource by its res:// path")
		_:
			return _bad("values of type " + kind + " are not part of the contract")


static func _basis(value) -> Dictionary:
	var axes: Array = []
	for key in ["x", "y", "z"]:
		var axis: Dictionary = _vec3(_member(value, key))
		if not axis["ok"]:
			return axis
		axes.append(axis["value"])
	return _good(Basis(axes[0], axes[1], axes[2]))
