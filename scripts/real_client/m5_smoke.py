"""M5 real-entry smoke: managed game and editor sessions on a real Godot build.

Usage:
    DISPLAY=:97 python3 -I -B scripts/real_client/m5_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK. The
fixture game renders with the Compatibility renderer, so a virtual X display
(Xvfb) is enough for windowed sessions and captures.

1. `project.run.start` answers a running session (the game keeps running);
   runtime tree, properties, signals (watched) and UI reads carry the session
   and a revision that moves when the scene changes.
2. Action, key and mouse input change the game; two captures of the same frame
   have the same dimensions and SHA-256; a recording is one animated PNG.
3. Input is released on every path: a hold expiring, a cancelled sequence (the
   client's `notifications/cancelled`), stopping the session, and the server
   going away (the game's own output proves the release; the next server
   reconciles the orphaned session and archives that output).
4. `editor.session.start` hosts the language server and debug adapter on ports
   the server allocated; `debug.command` reads the engine's own diagnostics and
   symbols, then hits a breakpoint and reads stack, scopes and variables —
   only through the editor's launch receipt; `editor.session.stop` ends it.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import contextlib
import hashlib
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp import types  # noqa: E402
from mcp_client import Server, connect  # noqa: E402

PROJECT = """config_version=5

[application]

config/name="m5"
run/main_scene="res://main.tscn"

[display]

window/size/viewport_width=320
window/size/viewport_height=240

[rendering]

renderer/rendering_method="gl_compatibility"
renderer/rendering_method.mobile="gl_compatibility"
"""

MAIN_SCENE = """[gd_scene load_steps=3 format=3]

[ext_resource type="Script" path="res://main.gd" id="1"]
[ext_resource type="Script" path="res://player.gd" id="2"]

[node name="Main" type="Node2D"]
script = ExtResource("1")

[node name="Background" type="ColorRect" parent="."]
offset_right = 320.0
offset_bottom = 240.0
color = Color(0.1, 0.2, 0.6, 1)

[node name="Player" type="Node2D" parent="."]
position = Vector2(40, 120)
script = ExtResource("2")

[node name="Label" type="Label" parent="."]
offset_left = 10.0
offset_top = 10.0
offset_right = 200.0
offset_bottom = 40.0
text = "hello"

[node name="Button" type="Button" parent="."]
offset_left = 200.0
offset_top = 180.0
offset_right = 300.0
offset_bottom = 220.0
text = "Press"
"""

MAIN = """extends Node2D

var last_key := ""
var clicks := 0

func _ready() -> void:
\t$Button.pressed.connect(_on_pressed)

func _on_pressed() -> void:
\tclicks += 1
\t$Label.text = "clicked %d" % clicks
\tvar spawned := Node2D.new()
\tspawned.name = "Spawned%d" % clicks
\tadd_child(spawned)

func _unhandled_input(event: InputEvent) -> void:
\tif event is InputEventKey and event.pressed and not event.echo:
\t\tlast_key = OS.get_keycode_string(event.keycode)
"""

# `_ready` holds the breakpoint line; `_process` reports every change of the
# held action, so the game's own output proves when input was released.
PLAYER = """extends Node2D

signal jumped

var speed := 120.0
var right_held := false

func _ready() -> void:
\tvar local_value := 41
\tspeed = speed + local_value - 41
\tprint("player ready ", local_value)

func _process(delta: float) -> void:
\tvar held := Input.is_action_pressed("ui_right")
\tif held != right_held:
\t\tright_held = held
\t\tprint("ui_right ", "pressed" if held else "released")
\tif held:
\t\tposition.x += speed * delta
\tif Input.is_action_just_pressed("ui_accept"):
\t\tjumped.emit()

func _draw() -> void:
\tdraw_rect(Rect2(-8, -8, 16, 16), Color(1, 0.8, 0.1))
"""
BREAKPOINT_LINE = 10  # `speed = speed + local_value - 41`

BROKEN = """extends Node

func broken() -> void:
\tvar x: int = "not an int"
"""


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


async def until(probe, what: str, timeout: float = 20.0, interval: float = 0.1):
    """Poll `probe` (an async callable) until it returns a truthy value."""
    deadline = time.monotonic() + timeout
    while True:
        value = await probe()
        if value:
            return value
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        await asyncio.sleep(interval)


async def health_planes(client) -> tuple[dict, dict, list]:
    health = (await client.call("health.get")).require_ok()
    components = {entry["component"]: entry for entry in health["components"]}
    planes = {entry["plane"]: entry for entry in health["planes"]}
    return components, planes, health["codes"]


def write_project(project: Path) -> None:
    project.mkdir()
    (project / "project.godot").write_text(PROJECT, encoding="utf-8")
    (project / "main.tscn").write_text(MAIN_SCENE, encoding="utf-8")
    (project / "main.gd").write_text(MAIN, encoding="utf-8")
    (project / "player.gd").write_text(PLAYER, encoding="utf-8")
    (project / "broken.gd").write_text(BROKEN, encoding="utf-8")


async def position_x(client, session: str) -> float:
    read = (await client.call("runtime.properties.get", {
        "session": session, "node": "Player", "properties": ["position"]})).require_ok()
    return float(read["properties"]["position"]["value"]["x"])


async def held(client, session: str) -> list:
    status = (await client.call("project.run.status", {"session": session})).require_ok()
    return status.get("held_input") or []


async def read_artifact(client, uri: str) -> bytes:
    read = await client.read_resource(uri)
    return base64.b64decode(read.contents[0].blob)


async def start_game(client, key: str) -> dict:
    started = (await client.call("project.run.start", {
        "idempotencyId": key, "display": "window", "resolution": {"width": 320, "height": 240}})).require_ok()
    assert started["session"]["state"] == "running", started
    assert started["session"]["probe"]["connected"] is True, started
    return started["session"]


async def game_flow(client, report: dict) -> None:
    # Before any session the runtime plane is attached but idle, and says how
    # to start one; it is never reported as absent from the build.
    components, planes, codes = await health_planes(client)
    assert components["runtime"]["status"] == "unavailable", components["runtime"]
    assert "project.run.start" in components["runtime"]["message"], components["runtime"]
    assert planes["runtime"]["boundary_compiled_in"] is True and planes["lsp_dap"]["boundary_compiled_in"] is True, planes
    assert "game_not_running" in codes and "runtime_plane_not_shipped" not in codes, codes
    started = await start_game(client, "m5-start")
    session = started["session"]
    assert session.startswith("run_") and len(session) == 36, session
    report["game"] = {"godot_version": started["godot_version"], "boundary_strength": started["boundary_strength"]}

    components, planes, codes = await health_planes(client)
    assert components["runtime"]["status"] == "ready" and planes["runtime"]["usable"] is True, (components, planes)
    assert "game_not_running" not in codes, codes
    report["game"]["health_runtime"] = components["runtime"]["status"]

    tree = (await client.call("runtime.tree.get", {"session": session, "depth": 3})).require_ok()
    assert tree["session"] == session and isinstance(tree["revision"], int), tree
    paths = {node["path"] for node in tree["nodes"]}
    assert "/root/Main/Player" in paths and "/root/Main/Button" in paths, sorted(paths)
    revision = tree["revision"]
    signals = (await client.call("runtime.signals.get", {
        "session": session, "node": "Player", "watch": True})).require_ok()
    jumped = next(entry for entry in signals["signals"] if entry["name"] == "jumped")
    assert jumped["emissions"] == 0, jumped
    ui = (await client.call("runtime.ui.get", {"session": session})).require_ok()
    texts = {entry.get("text") for entry in ui["controls"]}
    assert {"hello", "Press"} <= texts, ui

    # Two captures of the same, unchanged frame are the same image.
    first = (await client.call("runtime.screenshot.capture", {"session": session, "idempotencyId": "m5-shot-1"})).require_ok()
    second = (await client.call("runtime.screenshot.capture", {"session": session, "idempotencyId": "m5-shot-2"})).require_ok()
    for shot in (first, second):
        assert (shot["artifact"]["width"], shot["artifact"]["height"]) == (320, 240), shot
    assert first["artifact"]["sha256"] == second["artifact"]["sha256"], (first, second)
    png = await read_artifact(client, first["artifact"]["uri"])
    assert png[:8] == b"\x89PNG\r\n\x1a\n" and sha256(png) == first["artifact"]["sha256"], len(png)
    report["game"]["screenshot"] = {"width": 320, "height": 240, "sha256": first["artifact"]["sha256"],
                                    "bytes": len(png), "repeat_identical": True}

    # Action input moves the player while held; the hold expires by itself.
    before = await position_x(client, session)
    pressed = (await client.call("input.action.send", {
        "session": session, "idempotencyId": "m5-right", "action": "ui_right",
        "state": "press", "holdMs": 600})).require_ok()
    assert pressed["mutation_status"] == "committed", pressed
    assert [entry["input"] for entry in pressed["held"]] == ["action:ui_right"], pressed
    await until(lambda: held_empty(client, session), "the 600 ms hold to expire", timeout=10)
    moved = await position_x(client, session)
    assert moved > before + 10, (before, moved)
    settled = await position_x(client, session)
    await asyncio.sleep(0.3)  # one more look a few frames later: the player stays put
    assert await position_x(client, session) == settled, "the player still moves after its hold expired"
    report["game"]["expiry"] = {"moved_px": round(moved - before, 1)}

    # A tapped action emits the watched signal.
    (await client.call("input.action.send", {
        "session": session, "idempotencyId": "m5-jump", "action": "ui_accept", "state": "tap", "holdMs": 100})).require_ok()
    async def jump_counted():
        read = (await client.call("runtime.signals.get", {"session": session, "node": "Player"})).require_ok()
        return next(entry for entry in read["signals"] if entry["name"] == "jumped")["emissions"] == 1
    await until(jump_counted, "the watched signal to count one emission")

    # A key reaches the game's input handling.
    (await client.call("input.key.send", {
        "session": session, "idempotencyId": "m5-key", "key": "K", "state": "tap", "holdMs": 100})).require_ok()
    async def key_seen():
        read = (await client.call("runtime.properties.get", {
            "session": session, "node": ".", "properties": ["last_key"]})).require_ok()
        return read["properties"]["last_key"]["value"] == "K"
    await until(key_seen, "the game to record the key")

    # A mouse click on the button changes the UI and the scene tree.
    (await client.call("input.mouse.send", {
        "session": session, "idempotencyId": "m5-click", "event": {"button": {
            "position": {"x": 250, "y": 200}, "button": "left", "state": "tap",
            "holdMs": 100}}})).require_ok()
    async def clicked():
        read = (await client.call("runtime.ui.get", {"session": session})).require_ok()
        return "clicked 1" in {entry.get("text") for entry in read["controls"]}
    await until(clicked, "the button click to change the label")
    after = (await client.call("runtime.tree.get", {"session": session, "depth": 3})).require_ok()
    assert "/root/Main/Spawned1" in {node["path"] for node in after["nodes"]}, after
    assert after["revision"] > revision, (revision, after["revision"])
    report["game"]["inspection"] = {"revision_before": revision, "revision_after": after["revision"],
                                    "signal_emissions": 1, "key": "K", "ui": "clicked 1"}

    # A recording is one animated PNG.
    recording = (await client.call("runtime.recording.capture", {
        "session": session, "idempotencyId": "m5-rec", "durationMs": 600, "fps": 10})).require_ok()
    assert recording["artifact"]["media_type"] == "image/apng", recording
    apng = await read_artifact(client, recording["artifact"]["uri"])
    assert apng[:8] == b"\x89PNG\r\n\x1a\n" and b"acTL" in apng[:64], apng[:64]
    report["game"]["recording"] = {"frames": recording["artifact"]["frames"], "bytes": len(apng)}

    # A recording the client cancels is withdrawn from the probe: the session
    # answers its next read long before the eight-second recording would have
    # ended, and no recording is kept. The probe announces no start, so the
    # cancel is sent one second in: far longer than the request takes to reach
    # the probe over loopback, far shorter than the recording itself.
    async def artifact_uris() -> set[str]:
        listed = await client.session.list_resources()
        return {str(resource.uri) for resource in listed.resources}

    kept_before = await artifact_uris()
    recording_id: list[int] = []

    async def run_recording():
        recording_id.append(client.session._request_id)
        return await client.call("runtime.recording.capture", {
            "session": session, "idempotencyId": "m5-rec-cancel", "durationMs": 8000, "fps": 2})

    pending = asyncio.create_task(run_recording())
    await asyncio.sleep(1.0)
    withdrawn_at = time.monotonic()
    await client.session.send_notification(types.ClientNotification(types.CancelledNotification(
        params=types.CancelledNotificationParams(requestId=recording_id[0], reason="the smoke gave up"))))
    (await client.call("runtime.tree.get", {"session": session})).require_ok()
    answered_after = time.monotonic() - withdrawn_at
    pending.cancel()
    with contextlib.suppress(asyncio.CancelledError, Exception):
        await pending
    assert answered_after < 4.0, answered_after
    assert await artifact_uris() == kept_before, "a cancelled recording keeps no artifact"
    report["game"]["recording_cancel"] = {"next_read_after_s": round(answered_after, 2)}

    # Cancelling a sequence in flight releases what it pressed at once.
    sequence_id: list[int] = []

    async def run_sequence():
        # The SDK numbers the request synchronously when the call starts.
        sequence_id.append(client.session._request_id)
        return await client.call("input.sequence.send", {
            "session": session, "idempotencyId": "m5-sequence",
            "steps": [{"kind": "action", "action": "ui_right", "state": "press", "holdMs": 30000},
                      {"kind": "wait", "ms": 8000}]})

    # The probe runs one request at a time, so while the sequence runs the
    # game's own output (read from the server's capture, not the probe) is
    # what shows the press and the release.
    async def printed(line: str) -> int:
        logs = (await client.call("project.run.logs", {"session": session, "limit": 65536})).require_ok()
        return logs["text"].count(line)

    presses = await printed("ui_right pressed")
    releases = await printed("ui_right released")
    sequence = asyncio.create_task(run_sequence())

    async def pressed_by_sequence() -> bool:
        return await printed("ui_right pressed") > presses

    async def released_by_cancel() -> bool:
        return await printed("ui_right released") > releases

    await until(pressed_by_sequence, "the sequence to press ui_right")
    cancelled_at = time.monotonic()
    await client.session.send_notification(types.ClientNotification(types.CancelledNotification(
        params=types.CancelledNotificationParams(requestId=sequence_id[0], reason="the smoke gave up"))))
    await until(released_by_cancel, "the cancelled sequence to release ui_right", timeout=6)
    released_after = time.monotonic() - cancelled_at
    sequence.cancel()
    with contextlib.suppress(asyncio.CancelledError, Exception):
        await sequence
    assert released_after < 4.0, released_after
    assert await held_empty(client, session), "the probe still holds input after the cancelled sequence"
    report["game"]["cancel"] = {"released_after_s": round(released_after, 2)}

    # Stopping the session releases a hold that would have lasted 30 s.
    (await client.call("input.action.send", {
        "session": session, "idempotencyId": "m5-right-long", "action": "ui_right",
        "state": "press", "holdMs": 30000})).require_ok()
    logs = (await client.call("project.run.logs", {"session": session})).require_ok()
    assert "player ready 41" in logs["text"], logs
    stopped = (await client.call("project.run.stop", {"session": session, "idempotencyId": "m5-stop"})).require_ok()
    assert stopped["state"] == "exited", stopped
    assert [entry["input"] for entry in stopped["released_input"]] == ["action:ui_right"], stopped
    assert stopped["exit"]["boundary_empty"] is True, stopped
    archived = (await client.call("project.run.logs", {"session": session})).require_ok()
    text = archived["text"]
    assert archived["eof"] is True and text.rindex("ui_right released") > text.rindex("ui_right pressed"), text[-400:]
    report["game"]["stop"] = {"released": ["action:ui_right"], "termination": stopped["exit"]["termination"]}


async def held_empty(client, session: str) -> bool:
    return await held(client, session) == []


async def held_contains(client, session: str, key: str) -> bool:
    return key in [entry["input"] for entry in await held(client, session)]


def server_child() -> int:
    """The PID of the `godot-mcp serve` child of this process."""
    me = os.getpid()
    if sys.platform == "win32":
        listed = subprocess.run(
            ["powershell", "-NoProfile", "-Command",
             f"Get-CimInstance Win32_Process -Filter 'ParentProcessId={me}' | "
             "ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }"],
            capture_output=True, text=True, timeout=60, check=True).stdout
        children = [line.split(" ", 1) for line in listed.splitlines() if line.strip()]
    elif sys.platform == "darwin":
        listed = subprocess.run(["/bin/ps", "-A", "-o", "pid=,ppid=,command="],
                                capture_output=True, text=True, timeout=60, check=True).stdout
        children = [[pid, command] for pid, parent, command in
                    (line.split(None, 2) for line in listed.splitlines() if len(line.split(None, 2)) == 3)
                    if int(parent) == me]
    else:
        children = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                stat = (entry / "stat").read_text()
                cmdline = (entry / "cmdline").read_bytes().replace(b"\0", b" ")
            except OSError:
                continue
            if int(stat.rsplit(")", 1)[1].split()[1]) == me:
                children.append([entry.name, cmdline.decode(errors="replace")])
    for pid, command in children:
        if " serve" in f" {command}":
            return int(pid)
    raise AssertionError(f"no server child process among {children}")


def launch_capture(project: Path) -> list[Path]:
    return sorted(project.glob(".godot-mcp/**/launch-*/stdout"))


async def orphan_flow(arguments, project: Path, report: dict) -> None:
    server = Server(binary=arguments.binary, project=project, profile="full", godot=arguments.godot)
    session = None
    with contextlib.suppress(Exception):
        async with connect(server) as client:
            session = (await start_game(client, "m5-orphan"))["session"]
            (await client.call("input.action.send", {
                "session": session, "idempotencyId": "m5-orphan-right", "action": "ui_right",
                "state": "press", "holdMs": 30000})).require_ok()
            assert await held_contains(client, session, "action:ui_right")
            # TerminateProcess on Windows, which has no SIGKILL.
            os.kill(server_child(), getattr(signal, "SIGKILL", signal.SIGTERM))
            await asyncio.sleep(0)
    assert session is not None
    captures = launch_capture(project)
    assert captures, "the orphaned game's output capture is missing"
    # A Job Object is kill-on-close: on Windows the game ends with its server,
    # before its probe can let go. Elsewhere the game outlives its server, and
    # its own output says when its probe let go.
    released = sys.platform != "win32"
    if released:
        deadline = time.monotonic() + 10
        while not any("ui_right released" in path.read_text(errors="replace") for path in captures):
            assert time.monotonic() < deadline, "the probe did not release input when its server went away"
            await asyncio.sleep(0.1)
    async with connect(server) as client:
        status = (await client.call("project.run.status", {"session": session})).require_ok()
        assert status["state"] == "reconciled", status
        assert "proven empty" in status["reason"], status
        logs = (await client.call("project.run.logs", {"session": session})).require_ok()
        text = logs["text"]
        assert "ui_right pressed" in text, text[-600:]
        if released:
            assert "ui_right released" in text, text[-600:]
            assert text.index("ui_right released") > text.index("ui_right pressed"), text[-600:]
    report["orphan"] = {"state": "reconciled", "released_before_termination": released}


async def debug_flow(client, report: dict) -> None:
    early = await client.call("debug.session.get", {})
    assert early.is_error, early.raw_text
    started = (await client.call("editor.session.start", {"idempotencyId": "m5-editor"})).require_ok()
    editor = started["session"]["session"]
    assert editor.startswith("editor_") and started["session"]["state"] == "running", started
    _, planes, _ = await health_planes(client)
    assert planes["lsp_dap"]["usable"] is True, planes["lsp_dap"]
    described = (await client.call("debug.session.get", {"session": editor})).require_ok()
    assert described["language_server"]["receipt"]["owned"] is True, described
    assert described["debug_adapter"]["receipt"]["owned"] is True, described

    def command(name: str, **extra):
        return client.call("debug.command", {
            "session": editor, "command": {name: extra},
            "idempotencyId": f"m5-debug-{name}-{time.monotonic_ns()}"})

    diagnostics = (await command("diagnostics", path="broken.gd")).require_ok()
    found = diagnostics["diagnostics"]
    assert found and found[0]["severity"] == "error" and found[0]["line"] == 4, diagnostics
    clean = (await command("diagnostics", path="player.gd")).require_ok()
    assert clean["diagnostics"] == [], clean
    symbols = (await command("symbols", path="player.gd")).require_ok()
    names = {symbol["name"] for symbol in symbols["symbols"]}
    assert {"_ready", "_process", "speed", "jumped"} <= names, names

    marks = (await command("breakpoints", path="player.gd", lines=[BREAKPOINT_LINE])).require_ok()
    assert marks["breakpoints"] and marks["breakpoints"][0]["verified"] is True, marks
    launched = (await command("launch")).require_ok()
    assert launched["launch_state"] == "started", launched
    stopped = (await command("stopped", timeoutMs=60000)).require_ok()
    assert stopped["stopped"]["reason"] == "breakpoint", stopped
    stack = (await command("stack")).require_ok()
    top = stack["frames"][0]
    assert top["source"] == "res://player.gd" and top["line"] == BREAKPOINT_LINE, stack
    scopes = (await command("scopes", frame=top["id"])).require_ok()
    locals_scope = next(scope for scope in scopes["scopes"] if scope["name"] == "Locals")
    variables = (await command("variables", reference=locals_scope["reference"])).require_ok()
    local = next(variable for variable in variables["variables"] if variable["name"] == "local_value")
    assert local["value"] == "41", variables
    (await command("continue")).require_ok()
    (await command("pause")).require_ok()
    paused = (await command("stopped", timeoutMs=30000)).require_ok()
    assert paused["stopped"]["reason"] in ("pause", "paused"), paused
    (await command("stop")).require_ok()
    state = (await client.call("debug.session.get", {"session": editor})).require_ok()
    assert state["debug_adapter"]["breakpoints"] == {"res://player.gd": [BREAKPOINT_LINE]}, state
    closed = (await client.call("editor.session.stop", {"session": editor, "idempotencyId": "m5-editor-stop"})).require_ok()
    assert closed["state"] == "exited" and closed["exit"]["boundary_empty"] is True, closed
    late = await command("symbols", path="player.gd")
    assert late.is_error, late.raw_text
    report["debug"] = {
        "diagnostic": found[0]["message"],
        "symbols": sorted(names & {"_ready", "_process", "speed", "jumped"}),
        "breakpoint": {"line": top["line"], "source": top["source"], "local_value": local["value"]},
        "pause_reason": paused["stopped"]["reason"],
        "editor_exit": closed["exit"]["termination"],
    }


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    # macOS and Windows draw through their own desktop session, which the server
    # checks itself; only X11 and Wayland hosts name a display in the environment.
    if sys.platform not in ("darwin", "win32") and not (
            os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")):
        raise SystemExit("set DISPLAY (for example an Xvfb display): windowed sessions and captures need one")
    report: dict = {"godot": str(arguments.godot.name)}
    # Windows deletes a file only once its last handle closes, so a tree the
    # engine wrote until a moment ago may refuse a prompt removal.
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m5-", ignore_cleanup_errors=sys.platform == "win32") as scratch:
        scratch_path = Path(scratch).resolve()
        project = scratch_path / "game"
        write_project(project)
        server = Server(binary=arguments.binary, project=project, profile="full", godot=arguments.godot)
        # Each flow runs even when an earlier one failed, so one run reports
        # every failure; the smoke still fails on the first of them.
        failures: list[BaseException] = []
        async with connect(server) as client:
            for flow in (game_flow, debug_flow):
                try:
                    await flow(client, report)
                except Exception as error:  # noqa: BLE001 - reported and re-raised below
                    traceback.print_exception(error)
                    failures.append(error)
            transcript = "\n".join(client.transcript)
            assert str(project) not in transcript, "an answer named the private project path"
        orphan = scratch_path / "orphan"
        write_project(orphan)
        try:
            await orphan_flow(arguments, orphan, report)
        except Exception as error:  # noqa: BLE001 - reported and re-raised below
            traceback.print_exception(error)
            failures.append(error)
    print(json.dumps(report, indent=2, sort_keys=True))
    if failures:
        raise failures[0]
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
