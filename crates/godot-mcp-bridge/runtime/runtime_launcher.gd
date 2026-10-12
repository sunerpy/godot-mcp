extends SceneTree
## The entry point of a game session the godot-mcp server manages.
##
## Godot runs this with `--script` from the server's private session directory
## instead of starting the project's main scene itself. The project's autoloads
## are already in the tree; the launcher adds the session's runtime probe as
## the next child of the root, then loads the scene the server asked for — the
## project's main scene unless `GODOT_MCP_RUNTIME_SCENE` names another — and
## makes it the current scene, exactly where Godot would have put it. Nothing
## is written into the project.

const Probe = preload("runtime_probe.gd")
const SCENE_ENV := "GODOT_MCP_RUNTIME_SCENE"


func _initialize() -> void:
	var probe: Node = Probe.new()
	probe.name = "GodotMcpRuntimeProbe"
	root.add_child(probe)
	var path := OS.get_environment(SCENE_ENV)
	if path == "":
		path = str(ProjectSettings.get_setting("application/run/main_scene", ""))
	if path == "":
		printerr("godot-mcp: the project has no main scene and the session named none")
		quit(1)
		return
	var packed = load(path)
	if packed == null or not (packed is PackedScene):
		printerr("godot-mcp: %s is not a scene this project can load" % path)
		quit(1)
		return
	var scene: Node = packed.instantiate()
	root.add_child(scene)
	current_scene = scene
