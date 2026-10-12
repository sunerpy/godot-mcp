extends SceneTree
## The shared tagged-value goldens, GDScript side: the godot_mcp add-on's
## variant_codec.gd decodes every golden vector to the named Variant type and
## encodes it back to an equal tagged value, and refuses every golden refusal
## with the expected reason. Run headless with the add-on, the golden file and
## res://codec/sample.tres in the project:
##     godot --headless --path <project> --script res://variant_codec_probe.gd

const VariantCodec = preload("res://addons/godot_mcp/variant_codec.gd")


func _same(left, right) -> bool:
	var numbers := [TYPE_INT, TYPE_FLOAT]
	if numbers.has(typeof(left)) and numbers.has(typeof(right)):
		return float(left) == float(right)
	if typeof(left) != typeof(right):
		return false
	match typeof(left):
		TYPE_DICTIONARY:
			if left.size() != right.size():
				return false
			for key in left.keys():
				if not right.has(key) or not _same(left[key], right[key]):
					return false
			return true
		TYPE_ARRAY:
			if left.size() != right.size():
				return false
			for index in left.size():
				if not _same(left[index], right[index]):
					return false
			return true
	return left == right


func _init() -> void:
	var document = JSON.parse_string(FileAccess.get_file_as_string("res://variant-codec-v1.json"))
	var failures: Array = []
	for vector in document["vectors"]:
		var name: String = vector["name"]
		var decoded: Dictionary = VariantCodec.decode(vector["tagged"])
		if not decoded["ok"]:
			failures.append({"name": name, "stage": "decode", "reason": decoded["reason"]})
			continue
		var actual: String = type_string(typeof(decoded["value"]))
		if actual != vector["variant_type"]:
			failures.append({"name": name, "stage": "type", "actual": actual})
			continue
		var encoded: Dictionary = VariantCodec.encode(decoded["value"])
		if not encoded["ok"] or not _same(encoded["value"], vector["tagged"]):
			failures.append({"name": name, "stage": "encode", "encoded": encoded})
	for refusal in document["refusals"]:
		var decoded: Dictionary = VariantCodec.decode(refusal["tagged"])
		if decoded["ok"] or not str(decoded.get("reason", "")).contains(refusal["reason"]):
			failures.append({"name": refusal["name"], "stage": "refusal", "decoded": str(decoded)})
	print("VARIANT_CODEC_PROBE ", JSON.stringify({
		"vectors": document["vectors"].size(),
		"refusals": document["refusals"].size(),
		"failures": failures,
	}))
	quit(0)
