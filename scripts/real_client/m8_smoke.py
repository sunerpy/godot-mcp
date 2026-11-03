"""M8 real-entry smoke: an agent operating a live Godot editor through MCP.

Usage:
    xvfb-run -a -s "-screen 0 1280x800x24" \\
        python3 -I -B scripts/real_client/m8_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary (`godot-mcp-supervisor` beside it) through the
official MCP Python SDK the way an agent works in a running editor. The editor runs
windowed, drawn by the OpenGL renderer, so it needs a display; Xvfb is enough. With
`--headless` it runs without a window, and the capture and run steps check that they
are refused instead.

1. `editor.addon.install` writes and enables the bundled add-on; the editor starts and
   the plugin connects at the bundled version.
2. `editor.scene.open` opens a scene whose tool script prints and warns in the editor;
   `editor.log.get` reads both entries after the cursor taken before (Godot 4.5.1 and
   later; on 4.4.1 it answers `unsupported`). `editor.node.get` reads a node in full;
   `editor.scene.edit` changes the scene the editor holds and saves nothing;
   `editor.undo` returns to the pre-edit digest.
3. A second `editor.scene.edit` paints a tile and keys an animation with `call_method`
   operations (a new library and animation, a track, two keys) and answers what every
   call returned; `editor.scene.save` writes the file, which holds the cell and the keys.
4. A script written headless (`script.create`) is picked up by `editor.filesystem.scan`,
   `editor.types.get` reads its class, and `editor.script.open` puts the caret where
   asked.
5. `project.settings.patch` is carried out by the editor: it renames the project and sets
   the main scene, and the file holds both. `editor.preferences.set` changes one of the
   editor's own settings, which its settings file then holds, and refuses a credential.
   `editor.shortcuts.set` rebinds a shortcut and restores its default, and the settings
   file lists it only while it differs (Godot 4.6.1 and later; earlier releases answer
   `unsupported`).
6. `editor.screenshot.capture` answers a PNG artifact that reads back through
   `resources/read` to its reported size and SHA-256.
7. `editor.play.start` runs the main scene as the editor's child process, and
   `editor.play.stop` stops it.
8. A managed game session reports its performance monitors (`runtime.performance.get`).
9. `editor.scene.close` closes the saved scene (Godot 4.5.1 and later; on 4.4.1 it answers
   `unsupported`).

No step may make the editor print an engine error, and no answer may name the project's
absolute path.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import os
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect  # noqa: E402

PLUGIN_CFG = Path(__file__).resolve().parents[2] / "crates" / "godot-mcp-bridge" / "addon" / "godot_mcp" / "plugin.cfg"

PROJECT = 'config_version=5\n\n[application]\n\nconfig/name="m8"\n'

GREETER = """@tool
extends Node

func _ready() -> void:
\tif Engine.is_editor_hint():
\t\tprint("m8: the greeter is ready in the editor")
\t\tpush_warning("m8: the greeter warns in the editor")
"""

MAIN = """[gd_scene format=3]

[ext_resource type="Script" path="res://scripts/greeter.gd" id="1_greeter"]

[sub_resource type="PlaceholderTexture2D" id="PlaceholderTexture2D_t"]
size = Vector2(32, 16)

[sub_resource type="TileSetAtlasSource" id="TileSetAtlasSource_s"]
texture = SubResource("PlaceholderTexture2D_t")
0:0/0 = 0
1:0/0 = 0

[sub_resource type="TileSet" id="TileSet_s"]
sources/0 = SubResource("TileSetAtlasSource_s")

[node name="Main" type="Node2D"]

[node name="Greeter" type="Node" parent="."]
script = ExtResource("1_greeter")

[node name="Layer" type="TileMapLayer" parent="."]
tile_set = SubResource("TileSet_s")

[node name="Sprite" type="Sprite2D" parent="."]

[node name="Animator" type="AnimationPlayer" parent="."]
"""

HERO = """class_name M8Hero
extends CharacterBody2D

@export var speed := 200.0
"""


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def tagged(kind: str, value) -> dict:
    return {"type": kind, "value": value}


def integer(value: int) -> dict:
    return tagged("integer", value)


def number(value: float) -> dict:
    return tagged("float", value)


def vector2(x: float, y: float) -> dict:
    return tagged("vector2", {"x": x, "y": y})


def vector2i(x: int, y: int) -> dict:
    return tagged("vector2i", {"x": x, "y": y})


def call(node: str, method: str, arguments: list, resource: dict | None = None) -> dict:
    operation = {"op": "call_method", "node": node, "method": method, "arguments": arguments}
    if resource is not None:
        operation["resource"] = resource
    return operation


WALK = {"animation": "walk"}

# One tile painted and one animation keyed, with what each call returns.
CALLS = [
    (call("Layer", "set_cell", [vector2i(1, 2), integer(0), vector2i(0, 0)]), None),
    (call("Animator", "add_animation_library",
          [tagged("string", ""), tagged("new_resource", {"class": "AnimationLibrary"})]), integer(0)),
    (call("Animator", "add_animation",
          [tagged("string", "walk"), tagged("new_resource", {"class": "Animation"})],
          {"library": ""}), integer(0)),
    (call("Animator", "add_track", [integer(0)], WALK), integer(0)),
    (call("Animator", "track_set_path", [integer(0), tagged("node_path", "Sprite:position")], WALK), None),
    (call("Animator", "track_insert_key", [integer(0), number(0.0), vector2(1.0, 1.0)], WALK), integer(0)),
    (call("Animator", "track_insert_key", [integer(0), number(1.0), vector2(50.0, 1.0)], WALK), integer(1)),
    (call("Animator", "set_length", [number(1.0)], WALK), None),
]


def bundled_version() -> str:
    for line in PLUGIN_CFG.read_text(encoding="utf-8").splitlines():
        if line.startswith("version="):
            return line.split("=", 1)[1].strip().strip('"')
    raise AssertionError(f"{PLUGIN_CFG} names no version")


class Editor:
    """One editor process with private user directories, its output in a log."""

    def __init__(self, godot: Path, project: Path, home: Path, windowed: bool) -> None:
        for directory in ("config", "data", "cache"):
            (home / directory).mkdir(parents=True, exist_ok=True)
        self.log = home / "editor.log"
        environment = {
            "HOME": str(home),
            "PATH": "/usr/bin:/bin",
            "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_DATA_HOME": str(home / "data"),
            "XDG_CACHE_HOME": str(home / "cache"),
        }
        if windowed:
            for name in ("DISPLAY", "XAUTHORITY"):
                if name in os.environ:
                    environment[name] = os.environ[name]
            # Without a session bus address, a windowed editor autolaunches a bus of
            # its own, whose document portal mounts a file system inside `home` that
            # stays mounted until the X server exits. An address nothing listens on
            # keeps the editor off every bus.
            runtime = home / "runtime"
            runtime.mkdir(mode=0o700, exist_ok=True)
            environment["XDG_RUNTIME_DIR"] = str(runtime)
            environment["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={runtime / 'no-session-bus'}"
            arguments = ["--editor", "--rendering-driver", "opengl3", "--path", str(project)]
        else:
            arguments = ["--headless", "--editor", "--path", str(project)]
        self.process = subprocess.Popen(
            [str(godot), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=self.log.open("w", encoding="utf-8"),
            stderr=subprocess.STDOUT,
            env=environment,
        )

    def output(self) -> str:
        return self.log.read_text(encoding="utf-8", errors="replace")

    def children(self) -> list[int]:
        listed = subprocess.run(["pgrep", "-P", str(self.process.pid)], capture_output=True, text=True)
        return [int(line) for line in listed.stdout.split()]

    def errors_since(self, mark: int) -> list[str]:
        """The engine errors printed after `mark`. Three are not counted, as in the live
        suite: the headless dummy renderer's, asked for a texture while Godot 4.4 saves a
        scene's thumbnail, and two of the game the editor runs, which shares its output: X
        `BadWindow` errors of a game a 4.4.1 editor embeds, and the ALSA driver's failure
        to open a sound device on a host without one."""
        lines = self.output()[mark:].splitlines()
        exempt_sources = ("servers/rendering/dummy/", "drivers/alsa/")
        return [
            line for index, line in enumerate(lines)
            if "ERROR:" in line
            and "Unhandled XServer error: BadWindow" not in line
            and not (index + 1 < len(lines) and any(source in lines[index + 1] for source in exempt_sources))
        ]

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=30)


async def until(what: str, probe, timeout: float):
    """`probe()` awaited until it answers something, at most `timeout` seconds."""
    deadline = time.monotonic() + timeout
    while True:
        answer = await probe()
        if answer:
            return answer
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        await asyncio.sleep(0.25)


def png_size(data: bytes) -> tuple[int, int]:
    assert data[:8] == b"\x89PNG\r\n\x1a\n" and data[12:16] == b"IHDR", data[:16]
    return struct.unpack(">II", data[16:24])


async def captured(client, arguments: dict) -> dict:
    shot = (await client.call("editor.screenshot.capture", arguments)).require_ok()
    artifact = shot["artifact"]
    read = await client.session.read_resource(artifact["uri"])
    data = base64.b64decode(read.contents[0].blob)
    assert len(data) == artifact["byte_length"], artifact
    assert sha256(data) == artifact["sha256"], artifact
    assert png_size(data) == (artifact["width"], artifact["height"]), artifact
    return {"width": artifact["width"], "height": artifact["height"], "bytes": len(data)}


def entry_mentioning(page: dict, text: str) -> dict | None:
    for entry in page["entries"]:
        if text in (entry.get("text") or "") or text in (entry.get("code") or ""):
            return entry
    return None


async def flow(arguments: argparse.Namespace, scratch: Path, report: dict) -> None:
    project = scratch / "game"
    (project / "scripts").mkdir(parents=True)
    (project / "project.godot").write_text(PROJECT, encoding="utf-8")
    (project / "main.tscn").write_text(MAIN, encoding="utf-8")
    (project / "scripts" / "greeter.gd").write_text(GREETER, encoding="utf-8")
    windowed = not arguments.headless
    async with connect(Server(binary=arguments.binary, project=project, profile="full",
                              godot=arguments.godot)) as client:
        # 1. Install the add-on, start the editor, wait for the plugin.
        status = (await client.call("editor.addon.status")).require_ok()
        assert status["installed"] is False and status["install_allowed"] is True, status
        (await client.call("editor.addon.install", {"enable": True, "idempotencyId": "m8-install"})).require_ok()
        editor = Editor(arguments.godot, project, scratch / "home", windowed)
        try:
            async def connected():
                answer = (await client.call("editor.status.get")).require_ok()
                return answer if answer["connected"] else None

            try:
                status = await until("the plugin to connect", connected, 120)
            except AssertionError as error:
                raise AssertionError(f"{error}; editor exit={editor.process.poll()}") from None
            assert status["plugin_version"] == bundled_version(), status
            godot = status["godot_version"]
            logged = not godot.startswith("4.4.")
            assert status["editor"]["log_capture"] is logged, status
            report.update(godot=godot, plugin=status["plugin_version"], windowed=windowed)
            mark = len(editor.output())

            # 2. Open the scene, read the log, read a node, edit, undo.
            if logged:
                start = (await client.call("editor.log.get")).require_ok()
                cursor, epoch = start["next"], start["epoch"]
            else:
                refused = await client.call("editor.log.get")
                assert refused.is_error and refused.payload["code"] == "unsupported", refused.raw_text
                assert "4.5" in refused.raw_text, refused.raw_text
            opened = (await client.call("editor.scene.open", {
                "path": "main.tscn", "idempotencyId": "m8-open"})).require_ok()
            assert opened["edited_scene"] == "res://main.tscn", opened
            if logged:
                async def greeted():
                    page = (await client.call("editor.log.get", {"after": cursor, "epoch": epoch})).require_ok()
                    ready = entry_mentioning(page, "the greeter is ready")
                    warned = entry_mentioning(page, "the greeter warns")
                    return (ready, warned) if ready and warned else None

                ready, warned = await until("the tool script's log entries", greeted, 15)
                assert ready["kind"] == "message" and warned["kind"] == "warning", (ready, warned)
                report["log"] = {"message": ready["kind"], "warning": warned["kind"]}
            else:
                report["log"] = "unsupported before Godot 4.5"

            tree = (await client.call("editor.scene.tree.get")).require_ok()
            original = tree["digest"]
            layer = (await client.call("editor.node.get", {"node": "Layer"})).require_ok()
            assert layer["digest"] == original and layer["node"]["class"] == "TileMapLayer", layer["node"]
            assert any(prop["name"] == "tile_set" for prop in layer["properties"]), layer["properties"][:5]
            file_sha = sha256((project / "main.tscn").read_bytes())
            edited = (await client.call("editor.scene.edit", {
                "expectedDigest": original,
                "operations": [
                    {"op": "add_node", "parent": ".", "name": "Coin", "type": "Node2D"},
                    {"op": "set_property", "node": "Coin", "property": "position", "value": vector2(8.0, 9.0)},
                ],
                "label": "add a coin",
                "idempotencyId": "m8-edit-coin",
            })).require_ok()
            assert edited["digest"] != original and edited["action"] == "godot-mcp: add a coin", edited
            held = (await client.call("editor.scene.tree.get")).require_ok()
            assert held["digest"] == edited["digest"] and held["unsaved"] is True, held["unsaved"]
            assert sha256((project / "main.tscn").read_bytes()) == file_sha, "an edit saves nothing"
            undone = (await client.call("editor.undo", {"idempotencyId": "m8-undo"})).require_ok()
            assert undone["digest"] == original, undone

            # 3. Paint a tile and key an animation, then save.
            painted = (await client.call("editor.scene.edit", {
                "expectedDigest": original,
                "operations": [operation for operation, _ in CALLS],
                "label": "paint and key",
                "idempotencyId": "m8-edit-calls",
            })).require_ok()
            assert painted["results"] == [
                {"index": index, "method": operation["method"], "value": value}
                for index, (operation, value) in enumerate(CALLS)
            ], painted["results"]
            saved = (await client.call("editor.scene.save", {"idempotencyId": "m8-save"})).require_ok()
            scene = (project / "main.tscn").read_bytes()
            assert saved["saved"] == [{"path": "res://main.tscn", "sha256": sha256(scene)}], saved
            text = scene.decode("utf-8")
            for expected in ("tile_map_data = PackedByteArray(", 'NodePath("Sprite:position")',
                             "Vector2(50, 1)", "AnimationLibrary"):
                assert expected in text, f"{expected!r} is not in the saved scene:\n{text}"
            after = (await client.call("editor.scene.tree.get")).require_ok()
            assert after["digest"] == painted["digest"] and after["unsaved"] is False, after["unsaved"]
            report["edit"] = {"calls": len(CALLS), "undo_restores_digest": True, "saved_bytes": len(scene)}

            # 4. A script written headless, picked up by a scan.
            (await client.call("script.create", {
                "path": "scripts/hero.gd", "content": HERO, "idempotencyId": "m8-hero"})).require_ok()
            scanned = (await client.call("editor.filesystem.scan", {"idempotencyId": "m8-scan"})).require_ok()
            assert scanned["script_reload"] == "done", scanned
            hero = (await client.call("editor.types.get", {"class": "M8Hero"})).require_ok()
            assert hero["script"] == "res://scripts/hero.gd" and hero["inherits"][0] == "CharacterBody2D", hero
            assert any(prop["name"] == "speed" and prop["type"] == "float" for prop in hero["properties"]), hero
            caret = (await client.call("editor.script.open", {
                "path": "scripts/hero.gd", "line": 4, "column": 1, "idempotencyId": "m8-script-open"})).require_ok()
            assert caret["caret"] == {"line": 4, "column": 1}, caret
            report["scan"] = {"class": hero["class"], "caret": caret["caret"]}

            # 5. Project settings through the editor.
            settings = (await client.call("project.settings.get")).require_ok()
            patched = (await client.call("project.settings.patch", {
                "expectedSha256": settings["sha256"],
                "operations": [
                    {"op": "set_value", "setting": "application/config/name", "value": "m8 renamed"},
                    {"op": "set_main_scene", "scene": "res://main.tscn"},
                ],
                "idempotencyId": "m8-settings",
            })).require_ok()
            manifest = (project / "project.godot").read_bytes()
            assert patched["backend"] == "editor" and patched["after_sha256"] == sha256(manifest), patched
            assert b'config/name="m8 renamed"' in manifest and b"run/main_scene=" in manifest, manifest
            report["settings_backend"] = patched["backend"]

            # 5b. The editor's own settings: read, change, saved to its settings file.
            indent = "text_editor/behavior/indent/size"
            read = (await client.call("editor.preferences.get", {"settings": [indent]})).require_ok()
            entry = read["settings"][0]
            assert entry["value"] == integer(4) and entry["changeable"] is True, entry
            changed = (await client.call("editor.preferences.set", {
                "settings": {indent: integer(2)}, "idempotencyId": "m8-preferences"})).require_ok()
            assert changed["saved"] is True and changed["settings"][0]["after"] == integer(2), changed
            series = ".".join(godot.split(".")[:2])
            stored = (scratch / "home" / "config" / "godot" / f"editor_settings-{series}.tres").read_text(
                encoding="utf-8")
            assert f"{indent} = 2\n" in stored, stored
            withheld = (await client.call("editor.preferences.get", {"prefix": "export/"})).require_ok()
            assert withheld["settings"] and all(item.get("withheld") is True and "value" not in item
                                                for item in withheld["settings"]), withheld
            refused = await client.call("editor.preferences.set", {
                "settings": {"export/android/debug_keystore_pass": tagged("string", "x")},
                "idempotencyId": "m8-preferences-refused"})
            assert refused.is_error and refused.payload["code"] == "permission_denied", refused.raw_text
            report["preferences"] = {"changed": indent, "withheld": len(withheld["settings"])}

            # 5c. The editor's shortcuts: rebind one, saved, then back to its default.
            if (await client.call("editor.status.get")).require_ok()["editor"]["shortcuts"]:
                save = "script_editor/save"
                listed = (await client.call("editor.shortcuts.get", {"shortcuts": [save]})).require_ok()
                assert listed["shortcuts"][0]["changed"] is False, listed
                bound = (await client.call("editor.shortcuts.set", {
                    "shortcuts": {save: [{"key": "F9", "command_or_control": True}]},
                    "idempotencyId": "m8-shortcut"})).require_ok()
                assert bound["shortcuts"][0]["after"][0]["key"] == "F9", bound
                listing = f'"name": "{save}"'
                assert listing in (scratch / "home" / "config" / "godot" / f"editor_settings-{series}.tres").read_text(
                    encoding="utf-8")
                (await client.call("editor.shortcuts.set", {
                    "shortcuts": {save: "default"}, "idempotencyId": "m8-shortcut-default"})).require_ok()
                assert listing not in (scratch / "home" / "config" / "godot" / f"editor_settings-{series}.tres").read_text(
                    encoding="utf-8")
                report["shortcuts"] = {"rebound": save, "default": listed["shortcuts"][0]["events"][0]["text"]}
            else:
                refused = await client.call("editor.shortcuts.get", {})
                assert refused.is_error and refused.payload["code"] == "unsupported", refused.raw_text
                report["shortcuts"] = "unsupported by this Godot"

            # 6 and 7. Capture the editor, run the project, stop it.
            if windowed:
                (await client.call("editor.main_screen.set", {
                    "screen": "2D", "idempotencyId": "m8-screen"})).require_ok()
                report["captures"] = {
                    "window": await captured(client, {"idempotencyId": "m8-shot-window"}),
                    "2d": await captured(client, {"view": "2d", "maxWidth": 640, "idempotencyId": "m8-shot-2d"}),
                }
                running = (await client.call("editor.play.start", {"idempotencyId": "m8-play"})).require_ok()
                assert running["playing"] is True and running["scene"] == "res://main.tscn", running
                state = (await client.call("editor.status.get")).require_ok()["editor"]
                assert state["playing"] is True and state["playing_scene"] == "res://main.tscn", state
                assert editor.children(), "the game runs as the editor's child"
                stopped = (await client.call("editor.play.stop", {"idempotencyId": "m8-stop"})).require_ok()
                assert stopped["was_playing"] is True and stopped["playing"] is False, stopped

                async def childless():
                    return not editor.children()

                await until("the game to exit", childless, 10)
                report["play"] = {"scene": running["scene"], "saved": len(running["saved"]), "stopped": True}
            else:
                for name, call_arguments in (
                    ("editor.screenshot.capture", {"idempotencyId": "m8-shot-headless"}),
                    ("editor.play.start", {"idempotencyId": "m8-play-headless"}),
                ):
                    refused = await client.call(name, call_arguments)
                    assert refused.is_error and refused.payload["code"] == "unavailable", refused.raw_text
                    assert "headless" in refused.raw_text, refused.raw_text
                report["captures"] = report["play"] = "refused by a headless editor"

            # 8. A managed game session's performance monitors.
            started = (await client.call("project.run.start", {
                "scene": "main.tscn", "display": "headless", "idempotencyId": "m8-run"})).require_ok()
            session = started["session"]["session"]
            try:
                monitors = (await client.call("runtime.performance.get", {"session": session})).require_ok()
                assert isinstance(monitors["monitors"]["TIME_FPS"], (int, float)), monitors
                assert monitors["monitors"]["OBJECT_NODE_COUNT"] >= 5, monitors["monitors"]
                named = (await client.call("runtime.performance.get", {
                    "session": session, "monitors": ["OBJECT_NODE_COUNT"]})).require_ok()
                assert list(named["monitors"]) == ["OBJECT_NODE_COUNT"], named
            finally:
                (await client.call("project.run.stop", {"session": session, "idempotencyId": "m8-run-stop"})).require_ok()
            report["performance"] = {"monitors": len(monitors["monitors"]),
                                     "nodes": monitors["monitors"]["OBJECT_NODE_COUNT"]}

            # 9. Close the scene the editor holds, saved, where Godot can.
            state = (await client.call("editor.status.get")).require_ok()["editor"]
            if state["scene_close"]:
                closed = (await client.call("editor.scene.close", {"idempotencyId": "m8-close"})).require_ok()
                assert closed["closed"] == "res://main.tscn" and closed["open_scenes"] == [], closed
                report["close"] = closed["closed"]
            else:
                refused = await client.call("editor.scene.close", {"idempotencyId": "m8-close"})
                assert refused.is_error and refused.payload["code"] == "unsupported", refused.raw_text
                report["close"] = "unsupported by this Godot"

            errors = editor.errors_since(mark)
            assert not errors, f"the editor printed {len(errors)} engine errors, the first: {errors[:3]}"
            transcript = "\n".join(client.transcript)
            assert str(project) not in transcript, "an answer named the project's path"
            assert str(scratch / "home") not in transcript, "an answer named a path of the editor's user"
        except BaseException:
            editor.stop()
            print(f"editor exit={editor.process.returncode}; editor log tail:\n{editor.output()[-6000:]}",
                  file=sys.stderr)
            raise
        finally:
            editor.stop()


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    parser.add_argument("--headless", action="store_true",
                        help="run the editor without a window and check the capture and run refusals")
    arguments = parser.parse_args()
    if not arguments.headless and "DISPLAY" not in os.environ:
        parser.error("a windowed editor needs a display: run under xvfb-run, or pass --headless")
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    report: dict = {"godot_binary": arguments.godot.name}
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m8-") as scratch:
        await flow(arguments, Path(scratch).resolve(), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
