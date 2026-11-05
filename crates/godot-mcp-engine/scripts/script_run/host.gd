extends SceneTree
## The godot-mcp `script.run` host.
##
## A supervised headless Godot runs this file as its main loop (`--script`) with
## two user arguments: the `res://` file holding the caller's source and the
## `res://` file the host writes its one report to. Once the scene tree and the
## project's autoloads are up, it compiles the source, calls its `run()`,
## awaiting it when it is a coroutine, writes the report and quits.
##
## The report is a JSON object. `stage` is `compile` when the source did not
## compile, cannot be instantiated or defines no `run()`, and `done` when
## `run()` returned; `literal` is what it returned, spelled by Godot's own
## `var_to_str`, or null with `literal_length` when that spelling is longer
## than MAX_LITERAL. A run that never returns writes no report.

## The longest return-value spelling a report carries, in characters.
const MAX_LITERAL := 262144


func _initialize() -> void:
	_run.call_deferred()


func _report(path: String, report: Dictionary) -> void:
	var file := FileAccess.open(path, FileAccess.WRITE)
	if file == null:
		printerr("godot-mcp script host: cannot write the report: %s" % error_string(FileAccess.get_open_error()))
		return
	file.store_string(JSON.stringify(report))
	file.close()


func _run() -> void:
	# The project's autoloads enter the tree after this script's first frame.
	await process_frame
	var arguments := OS.get_cmdline_user_args()
	if arguments.size() != 2:
		printerr("godot-mcp script host: expected the source file and the report file")
		quit(2)
		return
	var report_path: String = arguments[1]
	var script := GDScript.new()
	script.source_code = FileAccess.get_file_as_string(arguments[0])
	var error := script.reload()
	if error != OK:
		_report(report_path, {"stage": "compile", "reason": "the source does not compile (%s)" % error_string(error)})
		quit(3)
		return
	if not script.can_instantiate():
		_report(report_path, {"stage": "compile", "reason": "the source cannot be instantiated"})
		quit(3)
		return
	var instance = script.new()
	if not instance.has_method("run"):
		_report(report_path, {"stage": "compile", "reason": "the source defines no run() method"})
		_release(instance)
		quit(3)
		return
	if instance is Node:
		root.add_child(instance)
	var value = await instance.run()
	var literal := var_to_str(value)
	if literal.length() > MAX_LITERAL:
		_report(report_path, {"stage": "done", "literal": null, "literal_length": literal.length()})
	else:
		_report(report_path, {"stage": "done", "literal": literal})
	_release(instance)
	quit(0)


func _release(instance) -> void:
	if not is_instance_valid(instance):
		return
	if instance is Node:
		instance.queue_free()
	elif not (instance is RefCounted):
		instance.free()
