#!/usr/bin/env python3
"""Take the documentation site's screenshots from the demo project, through the built server.

The site (docs/site/) shows six pictures, all taken by this script from
examples/site-demo/ with the server built from this tree:

    editor-en-light  editor-en-dark  editor-zh-light  editor-zh-dark  game  game-scenario

Each editor picture comes from a fresh windowed Godot editor with private user
directories. Its theme and language are written to its editor settings before it
starts and read back through `editor.preferences.get` once the add-on connects; the
Godot MCP panel is opened on its Activity page through `editor.script.run`, and a few
editor requests give that page something to show. The game picture comes from a
managed game session that `input.sequence.send` walks to two coins; a
`test.scenario.run` scenario then drives the game once more, and the screenshot its last
step keeps is the game-scenario picture.

Every answer is checked against the shape the server documents, and every picture is
read back and checked against its answer. Any failed check stops the run with a
non-zero exit that names the picture and the reason; nothing is written for a picture
that failed. The pictures are written to --out as lossless WebP, converted by the same
Godot (scripts/site/image_tool.gd), and the answers they came from to --report, which
the site's pages quote.

Run it from the repository root under a virtual display:

    xvfb-run -a -s "-screen 0 1600x1000x24" python3 -I -B scripts/site/capture_screens.py \\
      --binary target/release/godot-mcp --godot <the Godot editor binary> \\
      --out docs/site/public/screens

The Godot editor must be the latest release in tests/fixtures/godot-releases.lock.json;
the MCP Python SDK must be the version MCP_SDK names (`python3 -m pip install mcp==<it>`).
Look at every picture afterwards, one at a time.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import importlib.metadata
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DEMO = ROOT / "examples" / "site-demo"
LOCK = ROOT / "tests" / "fixtures" / "godot-releases.lock.json"
IMAGE_TOOL = Path(__file__).resolve().parent / "image_tool.gd"
# The version the real-client smokes (scripts/real_client/) are run with.
MCP_SDK = "1.28.1"

WINDOW = (1600, 1000)
GAME = (960, 540)
CONNECT_SECONDS = 120
DEADLINE_MS = 300_000

THEME = "interface/theme/color_preset"
LANGUAGE = "interface/editor/localization/editor_language"
# name, editor language, colour preset. The editor's own defaults are English ("auto" on a
# system without a locale) and the "Default" (dark) preset, so a variant writes only the
# settings that differ from them. Simplified Chinese is "zh_Hans" in the editor's list of
# languages; an unknown code leaves the editor in English, which the locale check catches.
EDITORS = [
    ("editor-en-light", "en", "Light"),
    ("editor-en-dark", "en", "Default"),
    ("editor-zh-light", "zh_Hans", "Light"),
    ("editor-zh-dark", "zh_Hans", "Default"),
]
# One game session takes both game pictures, so `--only game` takes the two of them.
GAME_PICTURES = ["game", "game-scenario"]
PICTURES = [name for name, _, _ in EDITORS] + ["game"]

# What clicking the dock's Details button does, plus choosing the Activity page. The node
# names are the add-on's own (crates/godot-mcp-bridge/addon/godot_mcp/).
OPEN_PANEL = """extends Node

func run():
\tvar base := EditorInterface.get_base_control()
\tvar dock := base.find_child("GodotMcp", true, false)
\tif dock == null:
\t\treturn {"opened": false, "reason": "no GodotMcp dock"}
\tvar button := dock.find_child("OpenPanel", true, false) as Button
\tif button == null:
\t\treturn {"opened": false, "reason": "no OpenPanel button"}
\tbutton.pressed.emit()
\tawait get_tree().process_frame
\tvar panel := base.find_child("GodotMcpPanel", true, false) as Control
\tif panel == null:
\t\treturn {"opened": false, "reason": "no GodotMcpPanel"}
\tvar tabs := panel.find_child("Tabs", true, false) as TabContainer
\tif tabs == null:
\t\treturn {"opened": false, "reason": "no Tabs"}
\ttabs.current_tab = 1  # Activity
\tawait get_tree().process_frame
\treturn {"opened": panel.is_visible_in_tree(), "tab": tabs.current_tab}
"""


# What a user does by dragging the bottom panel taller and zooming the 2D view to show the
# whole scene: the panel then lists more requests, and the scene is centred in the view. The
# panel redraws its Activity list at most every 0.25 s, so whether the newest request is drawn
# would be chance; the script redraws the list once and holds it for the screenshot.
FRAME_SCENE = """extends Node

const SCENE_SIZE := Vector2(%(width)d, %(height)d)
const PANEL_HEIGHT := 290.0
const RULER := 15.0

func run():
\tvar scale := EditorInterface.get_editor_scale()
\tvar base := EditorInterface.get_base_control()
\tvar panel := base.find_child("GodotMcpPanel", true, false) as Control
\tif panel == null:
\t\treturn {"framed": false, "reason": "no GodotMcpPanel"}
\tpanel.custom_minimum_size.y = PANEL_HEIGHT * scale
\tawait get_tree().process_frame
\tawait get_tree().process_frame
\tvar editor_2d: Node = null
\tfor child in EditorInterface.get_editor_main_screen().get_children():
\t\tif child.get_class() == "CanvasItemEditor":
\t\t\teditor_2d = child
\tif editor_2d == null:
\t\treturn {"framed": false, "reason": "no CanvasItemEditor"}
\tvar widgets := editor_2d.find_children("*", "EditorZoomWidget", true, false)
\tif widgets.is_empty():
\t\treturn {"framed": false, "reason": "no EditorZoomWidget"}
\t# The rulers cover the top and the left of the view.
\tvar visible := Vector2(EditorInterface.get_editor_viewport_2d().size) - Vector2(RULER, RULER) * scale
\tvar zoom := minf(1.0, minf(visible.x / SCENE_SIZE.x, visible.y / SCENE_SIZE.y) * 0.96)
\tvar widget = widgets[0]
\twidget.set_zoom(zoom)
\twidget.zoom_changed.emit(zoom)
\tawait get_tree().process_frame
\teditor_2d.center_at(SCENE_SIZE / 2 - Vector2(RULER, RULER) * scale / (2 * zoom))
\tawait get_tree().process_frame
\t# What the editor shows, not only what its settings say: the language it translates its
\t# interface into and how light its theme's base colour is.
\tvar base_color: Color = EditorInterface.get_editor_settings().get_setting("interface/theme/base_color")
\tpanel.refresh()
\tpanel.set_process(false)
\tawait get_tree().process_frame
\treturn {"framed": true, "zoom": zoom, "locale": TranslationServer.get_tool_locale(),
\t\t"luminance": base_color.get_luminance()}
""" % {"width": GAME[0], "height": GAME[1]}


class CaptureFailed(Exception):
    def __init__(self, picture: str, reason: str) -> None:
        super().__init__(f"{picture}: {reason}")
        self.picture = picture
        self.reason = reason


def check(picture: str, condition: bool, reason: str, answer=None) -> None:
    if not condition:
        detail = f" ({json.dumps(answer, sort_keys=True)[:1500]})" if answer is not None else ""
        raise CaptureFailed(picture, reason + detail)


def tagged(kind: str, value) -> dict:
    return {"type": kind, "value": value}


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def png_size(data: bytes) -> tuple[int, int]:
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        return (0, 0)
    return struct.unpack(">II", data[16:24])


def version_key(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("."))


def locked_version() -> str:
    """The newest Godot release the lock names: the one the pictures are taken with."""
    releases = json.loads(LOCK.read_text(encoding="utf-8"))["releases"]
    return max((release["version"] for release in releases), key=version_key)


def private_environment(home: Path) -> dict:
    """An environment whose every Godot user directory is inside `home`."""
    runtime = home / "runtime"
    for directory in ("config", "data", "cache"):
        (home / directory).mkdir(parents=True, exist_ok=True)
    runtime.mkdir(mode=0o700, exist_ok=True)
    environment = {
        "HOME": str(home),
        "PATH": "/usr/bin:/bin",
        "XDG_CONFIG_HOME": str(home / "config"),
        "XDG_DATA_HOME": str(home / "data"),
        "XDG_CACHE_HOME": str(home / "cache"),
        "XDG_RUNTIME_DIR": str(runtime),
        # Without a session bus address a windowed editor starts a bus of its own, whose
        # document portal mounts a file system inside `home` that stays mounted until the
        # X server exits. An address nothing listens on keeps the editor off every bus.
        "DBUS_SESSION_BUS_ADDRESS": f"unix:path={runtime / 'no-session-bus'}",
    }
    for name in ("DISPLAY", "XAUTHORITY"):
        if name in os.environ:
            environment[name] = os.environ[name]
    return environment


def godot_version(godot: Path, scratch: Path) -> str:
    done = subprocess.run([str(godot), "--headless", "--version"], capture_output=True, text=True,
                          env=private_environment(scratch / "version-home"), timeout=60)
    return done.stdout.strip()


class Editor:
    """One windowed editor with private user directories, its output in a log."""

    def __init__(self, godot: Path, project: Path, home: Path) -> None:
        environment = private_environment(home)
        self.log = home / "editor.log"
        width, height = WINDOW
        self.process = subprocess.Popen(
            [str(godot), "--editor", "--rendering-driver", "opengl3",
             "--resolution", f"{width}x{height}", "--position", "0,0", "--path", str(project)],
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

    def stop(self) -> list[int]:
        """Stop the editor and answer the child processes it still had."""
        children = self.children() if self.process.poll() is None else []
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=30)
        for child in children:
            subprocess.run(["kill", "-9", str(child)], capture_output=True)
        return children


async def until(picture: str, what: str, probe, timeout: float):
    """`probe()` awaited until it answers something, at most `timeout` seconds."""
    deadline = time.monotonic() + timeout
    while True:
        answer = await probe()
        if answer:
            return answer
        if time.monotonic() > deadline:
            raise CaptureFailed(picture, f"timed out after {timeout:.0f} s waiting for {what}")
        await asyncio.sleep(0.25)


async def read_artifact(client, uri: str, total: int, chunk: int = 4 * 1024 * 1024) -> bytes:
    from mcp import types

    data = bytearray()
    while len(data) < total:
        end = min(total, len(data) + chunk) - 1
        request = types.ClientRequest(types.ReadResourceRequest(params=types.ReadResourceRequestParams(
            uri=uri, _meta={"io.godot-mcp/artifactRange": {"start": len(data), "endInclusive": end}})))
        result = await client.session.send_request(request, types.ReadResourceResult)
        data.extend(base64.b64decode(result.contents[0].blob))
    return bytes(data)


async def picture_from(client, picture: str, answer: dict, size: tuple[int, int]) -> bytes:
    """The PNG an artifact answer names, checked against the answer and the expected size."""
    artifact = answer.get("artifact") or {}
    for field in ("uri", "byte_length", "sha256", "width", "height"):
        check(picture, field in artifact, f"the capture answer has no artifact.{field}", answer)
    data = await read_artifact(client, artifact["uri"], artifact["byte_length"])
    check(picture, len(data) == artifact["byte_length"], f"read {len(data)} bytes, the answer says "
          f"{artifact['byte_length']}")
    check(picture, sha256(data) == artifact["sha256"], "the bytes read do not have the answer's SHA-256")
    check(picture, png_size(data) == (artifact["width"], artifact["height"]),
          f"the PNG is {png_size(data)}, the answer says {(artifact['width'], artifact['height'])}")
    check(picture, png_size(data) == size, f"the picture is {png_size(data)}, not {size}")
    return data


def copy_demo(destination: Path) -> Path:
    shutil.copytree(DEMO, destination, ignore=shutil.ignore_patterns(".godot", ".godot-mcp"))
    return destination


def art_assets(project: Path) -> list[str]:
    return sorted(path.relative_to(project).as_posix() for path in (project / "art").glob("*.png"))


async def import_art(client, picture: str, project: Path) -> None:
    assets = art_assets(project)
    imported = (await client.call("project.import.run", {
        "assets": assets, "deadline_ms": DEADLINE_MS, "idempotencyId": f"{picture}-import"})).require_ok()
    check(picture, imported.get("classification") == "passed", "project.import.run did not pass", imported)
    states = {asset["path"]: asset["import"]["state"] for asset in imported["assets"]}
    check(picture, states == {asset: "imported" for asset in assets}, f"not every asset imported: {states}")


def settings_text(language: str, theme: str) -> str | None:
    lines = []
    if theme != "Default":
        lines.append(f'{THEME} = "{theme}"')
    if language != "en":
        lines.append(f'{LANGUAGE} = "{language}"')
    if not lines:
        return None
    return '[gd_resource type="EditorSettings" format=3]\n\n[resource]\n' + "\n".join(lines) + "\n"


async def capture_editor(arguments, scratch: Path, picture: str, language: str, theme: str,
                         settings_name: str, report: dict) -> bytes:
    from mcp_client import Server, connect

    work = scratch / picture
    project = copy_demo(work / "game")
    home = work / "home"
    text = settings_text(language, theme)
    if text is not None:
        (home / "config" / "godot").mkdir(parents=True)
        (home / "config" / "godot" / settings_name).write_text(text, encoding="utf-8")
    server = Server(binary=arguments.binary, project=project, profile="full", godot=arguments.godot,
                    extra_config={"allow_scripts": True})
    async with connect(server) as client:
        await import_art(client, picture, project)
        installed = (await client.call("editor.addon.install", {
            "enable": True, "idempotencyId": f"{picture}-addon"})).require_ok()
        editor = Editor(arguments.godot, project, home)
        try:
            data, facts = await drive_editor(client, picture, language, theme)
        except BaseException:
            editor.stop()
            raise
        left = editor.stop()
        check(picture, not left, f"the editor left child processes {left}")
        facts["addon"] = {"installed": installed.get("installed"), "enabled": installed.get("enabled")}
        report[picture] = facts
        return data


async def drive_editor(client, picture: str, language: str, theme: str) -> tuple[bytes, dict]:
    """Everything one editor picture needs once its editor is starting."""
    async def connected():
        answer = (await client.call("editor.status.get")).require_ok()
        return answer if answer.get("connected") else None

    status = await until(picture, "the add-on to connect", connected, CONNECT_SECONDS)

    # The theme and the language the editor runs with, read back from the editor.
    read = (await client.call("editor.preferences.get", {"settings": [THEME, LANGUAGE]})).require_ok()
    check(picture, not read.get("unknown"), "the editor does not know a setting", read)
    names = [entry.get("name") for entry in read.get("settings", [])]
    check(picture, names == [THEME, LANGUAGE], f"the editor answered {names}", read)
    values = [entry.get("value") for entry in read["settings"]]
    expected = [tagged("string", theme), tagged("string", language if language != "en" else "auto")]
    check(picture, values == expected, f"the editor runs with {values}, not {expected}", read)

    # The Godot MCP panel on its Activity page.
    opened = (await client.call("editor.script.run", {
        "source": OPEN_PANEL, "idempotencyId": f"{picture}-panel", "deadline_ms": 60_000})).require_ok()
    check(picture, opened.get("classification") == "passed", "editor.script.run did not pass", opened)
    check(picture, opened.get("result_unavailable") is None, "the panel script's result is unavailable",
          opened)
    check(picture, opened.get("result") == tagged("dictionary", {
        "opened": tagged("bool", True), "tab": tagged("integer", 1)}),
        "the panel did not open on its Activity page", opened)
    check(picture, opened.get("changes_unavailable") is None and opened["changes"]["total"] == 0,
          "the panel script changed project files", opened)

    # Requests for the Activity page to show, newest first, and a scene to look at; then the
    # view framed and the page held as it stands.
    scene = (await client.call("editor.scene.open", {
        "path": "main.tscn", "idempotencyId": f"{picture}-open"})).require_ok()
    check(picture, scene.get("edited_scene") == "res://main.tscn", "main.tscn did not open", scene)
    (await client.call("editor.main_screen.set", {
        "screen": "2D", "idempotencyId": f"{picture}-2d"})).require_ok()
    (await client.call("editor.selection.set", {
        "nodes": ["Player"], "idempotencyId": f"{picture}-select"})).require_ok()
    tree = (await client.call("editor.scene.tree.get")).require_ok()
    player = (await client.call("editor.node.get", {"node": "Player"})).require_ok()
    check(picture, player["node"]["class"] == "CharacterBody2D", "Player is not the demo's player",
          player["node"])
    moved = (await client.call("editor.scene.edit", {
        "expectedDigest": tree["digest"],
        "label": "Move a coin",
        "operations": [{"op": "set_property", "node": "Coins/CoinC", "property": "position",
                        "value": tagged("vector2", {"x": 410.0, "y": 440.0})}],
        "idempotencyId": f"{picture}-edit"})).require_ok()
    saved = (await client.call("editor.scene.save", {"idempotencyId": f"{picture}-save"})).require_ok()
    framed = (await client.call("editor.script.run", {
        "source": FRAME_SCENE, "idempotencyId": f"{picture}-frame", "deadline_ms": 60_000})).require_ok()
    check(picture, framed.get("classification") == "passed", "the framing script did not pass", framed)
    result = (framed.get("result") or {}).get("value") or {}
    check(picture, result.get("framed") == tagged("bool", True), "the scene was not framed", framed)
    zoom = (result.get("zoom") or {}).get("value")
    check(picture, isinstance(zoom, (int, float)) and 0.5 <= zoom <= 1.0,
          f"the 2D view's zoom is {zoom}, too small to read", framed)
    check(picture, framed.get("changes_unavailable") is None and framed["changes"]["total"] == 0,
          "the framing script changed project files", framed)
    locale = (result.get("locale") or {}).get("value", "")
    check(picture, locale.split("_")[0] == language.split("_")[0],
          f"the editor's interface is in {locale!r}, not {language!r}", framed)
    luminance = (result.get("luminance") or {}).get("value")
    check(picture, isinstance(luminance, (int, float)) and (luminance > 0.5) == (theme == "Light"),
          f"the editor's base colour has luminance {luminance}, which is not a {theme} theme", framed)

    shot = (await client.call("editor.screenshot.capture", {
        "idempotencyId": f"{picture}-shot"})).require_ok()
    data = await picture_from(client, picture, shot, WINDOW)
    return data, {
        "godot": status.get("godot_version"),
        "plugin": status.get("plugin_version"),
        "preferences": read["settings"],
        "panel": {"classification": opened["classification"], "result": opened["result"]},
        "zoom": zoom,
        "locale": locale,
        "luminance": luminance,
        "edit": {"label": "Move a coin", "digest_changed": moved.get("digest") != tree["digest"]},
        "saved": saved.get("saved"),
        "capture": {"call": {"name": "editor.screenshot.capture"},
                    "artifact": {key: shot["artifact"][key]
                                 for key in ("byte_length", "sha256", "width", "height")}},
    }


async def capture_game(arguments, scratch: Path, report: dict) -> dict[str, bytes]:
    from mcp_client import Server, connect

    picture = "game"
    project = copy_demo(scratch / picture / "game")
    server = Server(binary=arguments.binary, project=project, profile="full", godot=arguments.godot)
    async with connect(server) as client:
        await import_art(client, picture, project)
        width, height = GAME
        started = (await client.call("project.run.start", {
            "display": "window", "resolution": {"width": width, "height": height},
            "idempotencyId": "game-run"})).require_ok()
        session = started["session"]["session"]
        check(picture, started["session"]["state"] == "running", "the game is not running", started)
        check(picture, started["session"]["probe"]["connected"] is True, "the runtime probe is not connected",
              started)
        try:
            walked = (await client.call("input.sequence.send", {
                "session": session, "idempotencyId": "game-walk",
                "steps": [{"kind": "action", "action": "ui_right", "state": "press", "holdMs": 2000},
                          {"kind": "wait", "ms": 2000}]})).require_ok()
            wait_call = {"session": session, "timeoutMs": 5000, "condition": {"property": {
                "node": ".", "property": "score", "op": "==", "value": tagged("integer", 2)}}}
            waited = (await client.call("runtime.wait", wait_call)).require_ok()
            check(picture, waited.get("satisfied") is True and waited.get("outcome") == "satisfied",
                  "the score never reached 2", waited)

            shot_call = {"session": session, "idempotencyId": "game-shot"}
            shot = (await client.call("runtime.screenshot.capture", shot_call)).require_ok()
            data = await picture_from(client, picture, shot, GAME)

            scenario_call = {"session": session, "idempotencyId": "game-scenario", "steps": [
                {"kind": "assert_property", "id": "two-coins",
                 "target": {"node": ".", "property": "score"}, "op": "equals", "value": tagged("integer", 2)},
                {"kind": "input", "id": "walk-back",
                 "steps": [{"kind": "action", "action": "ui_left", "state": "press", "holdMs": 2500},
                           {"kind": "wait", "ms": 2500}]},
                {"kind": "wait", "id": "third-coin", "timeoutMs": 5000, "condition": {"property": {
                    "node": ".", "property": "score", "op": "==", "value": tagged("integer", 3)}}},
                {"kind": "assert_ui_text", "id": "hud", "node": "HUD/Count", "text": "3"},
                {"kind": "screenshot", "id": "after"},
            ]}
            scenario = (await client.call("test.scenario.run", scenario_call)).require_ok()
            counts = scenario.get("counts") or {}
            check(picture, scenario.get("verdict") == "passed", "the scenario did not pass", scenario)
            check(picture, counts.get("passed") == counts.get("total") == len(scenario_call["steps"]),
                  "not every scenario step passed", scenario)
            check(picture, all(counts.get(key) == 0 for key in ("failed", "error", "skipped", "uncertain")),
                  "a scenario step failed, errored, was skipped or is uncertain", scenario)
            last = scenario["steps"][-1]
            kept = [artifact for artifact in last.get("artifacts") or [] if artifact.get("role") == "screenshot"]
            check("game-scenario", last.get("id") == "after" and len(kept) == 1,
                  "the scenario's last step kept no single screenshot", last)
            after = await picture_from(client, "game-scenario", {"artifact": kept[0]}, GAME)
        finally:
            stopped = (await client.call("project.run.stop", {
                "session": session, "idempotencyId": "game-stop"})).require_ok()
        check(picture, stopped.get("state") == "exited", "the game did not exit", stopped)
        report[picture] = {
            "session": {"state": started["session"]["state"],
                        "probe_connected": started["session"]["probe"]["connected"]},
            "walk": {"steps_run": walked.get("steps_run"), "mutation_status": walked.get("mutation_status")},
            "wait": {"call": wait_call | {"session": "<session>"}, "answer": waited},
            "capture": {"call": {"name": "runtime.screenshot.capture",
                                 "arguments": shot_call | {"session": "<session>"}},
                        "artifact": {key: shot["artifact"][key]
                                     for key in ("byte_length", "sha256", "width", "height")}},
            "scenario": {"call": scenario_call | {"session": "<session>"},
                         "verdict": scenario["verdict"], "counts": counts,
                         "steps": [{"id": step.get("id"), "kind": step.get("kind"), "status": step.get("status")}
                                   for step in scenario.get("steps", [])],
                         "screenshot": {key: kept[0][key] for key in ("byte_length", "sha256", "width", "height")}},
            "stopped": stopped.get("state"),
        }
        return {"game": data, "game-scenario": after}


def to_webp(godot: Path, scratch: Path, pictures: dict[str, bytes], out: Path) -> dict[str, dict]:
    """Write each picture to `out` as lossless WebP through scripts/site/image_tool.gd."""
    tool = scratch / "image-tool"
    tool.mkdir()
    (tool / "project.godot").write_text("config_version=5\n", encoding="utf-8")
    shutil.copy(IMAGE_TOOL, tool / "image_tool.gd")
    pairs = []
    for name, data in pictures.items():
        source = scratch / f"{name}.png"
        source.write_bytes(data)
        pairs += [str(source), str(scratch / f"{name}.webp")]
    done = subprocess.run([str(godot), "--headless", "--path", str(tool), "--script", "res://image_tool.gd",
                           "--", "webp", *pairs], capture_output=True, text=True,
                          env=private_environment(scratch / "image-home"), timeout=300)
    if done.returncode != 0:
        raise CaptureFailed("webp", f"image_tool.gd exited {done.returncode}: {done.stderr.strip()[-800:]}")
    out.mkdir(parents=True, exist_ok=True)
    written = {}
    for name, data in pictures.items():
        webp = (scratch / f"{name}.webp").read_bytes()
        if webp[:4] != b"RIFF" or webp[8:12] != b"WEBP":
            raise CaptureFailed(name, "image_tool.gd did not write a WebP file")
        (out / f"{name}.webp").write_bytes(webp)
        width, height = png_size(data)
        written[name] = {"width": width, "height": height, "bytes": len(webp), "sha256": sha256(webp)}
    return written


async def run(arguments, scratch: Path, report: dict) -> dict[str, bytes]:
    settings_name = report["editor_settings_file"]
    pictures: dict[str, bytes] = {}
    for name, language, theme in EDITORS:
        if arguments.only and name not in arguments.only:
            continue
        print(f"capture_screens: {name}", flush=True)
        pictures[name] = await capture_editor(arguments, scratch, name, language, theme, settings_name,
                                              report["answers"])
    if not arguments.only or "game" in arguments.only:
        print("capture_screens: " + " and ".join(GAME_PICTURES), flush=True)
        pictures.update(await capture_game(arguments, scratch, report["answers"]))
    return pictures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", type=Path, required=True, help="the built godot-mcp binary")
    parser.add_argument("--godot", type=Path, required=True, help="the Godot editor binary")
    parser.add_argument("--out", type=Path, required=True, help="the directory the WebP pictures go to")
    parser.add_argument("--report", type=Path, default=ROOT / "target" / "site-captures" / "report.json",
                        help="where the answers the pictures came from are written")
    parser.add_argument("--only", action="append", choices=PICTURES,
                        help="take only this picture (repeatable; game takes both game pictures); a full run takes all six")
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()

    try:
        sdk = importlib.metadata.version("mcp")
    except importlib.metadata.PackageNotFoundError:
        sdk = None
    if sdk != MCP_SDK:
        print(f"capture_screens: the MCP Python SDK is {sdk or 'not installed'}, not {MCP_SDK}; "
              f"install it with: python3 -m pip install 'mcp=={MCP_SDK}'", file=sys.stderr)
        return 2
    if not os.environ.get("DISPLAY"):
        print("capture_screens: no DISPLAY; run it under xvfb-run (see the docstring)", file=sys.stderr)
        return 2
    sys.path.insert(0, str(ROOT / "scripts" / "real_client"))

    with tempfile.TemporaryDirectory(prefix="godot-mcp-screens-") as scratch_name:
        scratch = Path(scratch_name)
        locked = locked_version()
        found = godot_version(arguments.godot, scratch)
        if not (found == locked or found.startswith(locked + ".")):
            print(f"capture_screens: {arguments.godot} is Godot {found or '(no version)'}, not the latest "
                  f"locked release {locked}", file=sys.stderr)
            return 2
        major, minor = locked.split(".")[:2]
        report = {"godot": locked, "editor_settings_file": f"editor_settings-{major}.{minor}.tres",
                  "answers": {}}
        try:
            pictures = asyncio.run(run(arguments, scratch, report))
            written = to_webp(arguments.godot, scratch, pictures, arguments.out)
        except CaptureFailed as failure:
            print(f"capture_screens: {failure.picture} failed: {failure.reason}", file=sys.stderr)
            return 1
    report["pictures"] = written
    arguments.report.parent.mkdir(parents=True, exist_ok=True)
    arguments.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    for name, facts in written.items():
        print(f"{name}.webp {facts['width']}x{facts['height']} {facts['bytes']} bytes sha256 {facts['sha256']}")
    if arguments.only:
        print(f"capture_screens: took only {', '.join(written)}; a full run takes all six", file=sys.stderr)
    print(f"capture_screens: answers in {arguments.report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
