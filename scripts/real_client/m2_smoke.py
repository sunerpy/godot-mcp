"""M2 real-entry smoke: an empty workspace becomes an authored, imported project.

Usage:
    python3 -I -B scripts/real_client/m2_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK:

1. `serve --workspace` over an empty directory lists only the bootstrap tools;
   `project.create` and `project.bind` produce a bound project at revision 1
   and announce the changed tool list;
2. a script is created, patched, statically validated and engine-checked by a
   real Godot `--check-only` run;
3. a scene is created, then the main scene, an autoload and an input action
   are set through typed settings operations, and a real Godot process reads
   them back from the project (outside MCP);
4. a PNG is uploaded inline and a WAV in chunks, both are imported by a real
   Godot `--import`, and each asset reports the same source SHA-256 before
   and after import plus the hash of every imported artifact, with no engine
   cache path in any answer;
5. a fresh server bound to the same project reads the same settings back.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import io
import json
import struct
import subprocess
import sys
import tempfile
import wave
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect, private_godot_environment  # noqa: E402

BOOTSTRAP = {
    "capabilities.get", "health.get", "project.bind", "project.create",
    "workspace.projects.list",
}

PLAYER = """extends CharacterBody2D

signal jumped

func _physics_process(_delta: float) -> void:
\tif Input.is_action_just_pressed("jump"):
\t\tjumped.emit()
"""

GAME = """extends Node

var score := 0
"""

SCENE = """[gd_scene load_steps=2 format=3]

[ext_resource type="Script" path="res://scripts/player.gd" id="1"]

[node name="Main" type="Node2D"]

[node name="Player" type="CharacterBody2D" parent="."]
script = ExtResource("1")
"""

VERIFY = """extends SceneTree

func _init() -> void:
\tvar events := InputMap.action_get_events("jump") if InputMap.has_action("jump") else []
\tvar first_key := 0
\tif events.size() > 0 and events[0] is InputEventKey:
\t\tfirst_key = events[0].physical_keycode
\tprint("VERIFY ", JSON.stringify({
\t\t"main_scene": str(ProjectSettings.get_setting("application/run/main_scene", "")),
\t\t"autoload": str(ProjectSettings.get_setting("autoload/Game", "")),
\t\t"name": str(ProjectSettings.get_setting("application/config/name", "")),
\t\t"jump_events": events.size(),
\t\t"jump_first_physical_key": first_key,
\t\t"space": KEY_SPACE,
\t}))
\tquit(0)
"""


def png_bytes(width: int = 4, height: int = 4) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    rows = b"".join(b"\x00" + bytes([255, 0, 0, 255]) * width for _ in range(height))
    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows))
        + chunk(b"IEND", b"")
    )


def wav_bytes(frames: int = 22050) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as writer:
        writer.setnchannels(1)
        writer.setsampwidth(2)
        writer.setframerate(22050)
        writer.writeframes(b"".join(struct.pack("<h", (index * 37) % 2000 - 1000) for index in range(frames)))
    return buffer.getvalue()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


async def wait_for_notification(client, method: str) -> None:
    while method not in client.notifications:
        await asyncio.sleep(0.01)


async def bootstrap_and_author(arguments: argparse.Namespace, workspace: Path, report: dict) -> None:
    server = Server(binary=arguments.binary, workspace=workspace, profile="full", godot=arguments.godot)
    async with connect(server) as client:
        capabilities = client.initialized.capabilities
        assert capabilities.tools is not None and capabilities.tools.listChanged, capabilities
        assert set(await client.tool_names()) == BOOTSTRAP, await client.tool_names()

        created = (await client.call("project.create", {
            "path": "game", "name": "M2 Smoke", "renderer": "gl_compatibility",
            "idempotencyId": "m2-create",
        })).require_ok()
        assert created["path"] == "game", created
        bound = (await client.call("project.bind", {"path": "game"})).require_ok()
        assert bound["bound"] is True and bound["workspace"]["revision"] == 1, bound
        # The notification precedes the bind response on the wire; the SDK may
        # hand it to the handler a scheduling step later.
        await asyncio.wait_for(wait_for_notification(client, "notifications/tools/list_changed"), 5)
        names = set(await client.tool_names())
        assert "file.create" in names and "project.bind" not in names, sorted(names)
        report["bootstrap"] = {"bound_revision": bound["workspace"]["revision"], "tools_after_bind": len(names)}

        # --- source authoring -------------------------------------------------
        script = (await client.call("script.create", {
            "path": "scripts/player.gd", "content": PLAYER.replace("jumped.emit()", "pass"),
            "idempotencyId": "m2-script-create",
        })).require_ok()
        assert script["static_check"]["verdict"] == "valid_static", script
        patched = (await client.call("script.patch", {
            "path": "scripts/player.gd", "expectedSha256": script["after_sha256"],
            "change": {"content": PLAYER}, "idempotencyId": "m2-script-patch",
        })).require_ok()
        assert patched["after_sha256"] == sha256(PLAYER.encode()), patched
        validated = (await client.call("script.validate", {"script": "scripts/player.gd"})).require_ok()
        assert validated["data"]["verdict"] == "valid_static", validated
        checked = (await client.call("script.check", {"script": "scripts/player.gd"})).require_ok()
        assert checked["classification"] == "passed", checked
        (await client.call("script.create", {
            "path": "scripts/game.gd", "content": GAME, "idempotencyId": "m2-game",
        })).require_ok()
        scene = (await client.call("file.create", {
            "path": "scenes/main.tscn", "content": SCENE, "idempotencyId": "m2-scene",
        })).require_ok()
        report["source"] = {
            "script_sha256": patched["after_sha256"],
            "engine_check": checked["classification"],
            "scene_revision": scene["workspace"]["revision"],
        }

        # --- typed settings ---------------------------------------------------
        settings = (await client.call("project.settings.get")).require_ok()
        changed = (await client.call("project.settings.patch", {
            "expectedSha256": settings["sha256"],
            "operations": [
                {"op": "set_main_scene", "scene": "res://scenes/main.tscn"},
                {"op": "set_autoload", "name": "Game", "path": "res://scripts/game.gd"},
                {"op": "set_input_action", "name": "jump", "events": [
                    {"type": "key", "key": "Space"},
                    {"type": "mouse_button", "button": "left"},
                ]},
                {"op": "set_value", "setting": "application/config/name", "value": "M2 Renamed"},
            ],
            "idempotencyId": "m2-settings",
        })).require_ok()
        assert changed["conclusion"] == "proven_post_state", changed

        # --- assets -----------------------------------------------------------
        png = png_bytes()
        put_png = (await client.call("asset.put", {
            "path": "art/icon.png", "contentBase64": b64(png), "sha256": sha256(png),
            "idempotencyId": "m2-png",
        })).require_ok()
        assert put_png["conclusion"] == "proven_post_state", put_png
        wav = wav_bytes()
        third = len(wav) // 3 + 1
        upload = None
        last = None
        for index, offset in enumerate(range(0, len(wav), third)):
            arguments_ = {
                "path": "audio/blip.wav", "contentBase64": b64(wav[offset:offset + third]),
                "sha256": sha256(wav), "totalBytes": len(wav),
                "idempotencyId": f"m2-wav-{index}",
            }
            if upload is not None:
                arguments_["upload"] = {"id": upload, "offset": offset}
            last = (await client.call("asset.put", arguments_)).require_ok()
            upload = last.get("upload_id", upload)
        assert last is not None and last["complete"] is True and last["conclusion"] == "proven_post_state", last

        before = {
            path: (await client.call("asset.status", {"path": path})).require_ok()
            for path in ("art/icon.png", "audio/blip.wav")
        }
        assert all(status["import"]["state"] == "not_imported" for status in before.values()), before
        imported = (await client.call("project.import.run", {
            "assets": ["art/icon.png", "audio/blip.wav"], "idempotencyId": "m2-import",
        })).require_ok()
        assert imported["classification"] == "passed", json.dumps(imported, indent=2)
        after = {asset["path"]: asset for asset in imported["assets"]}
        for path, status in before.items():
            assert after[path]["sha256"] == status["sha256"], (path, status, after[path])
            assert after[path]["import"]["state"] == "imported", after[path]
            assert after[path]["import"]["source_matches_import"] is True, after[path]
            assert after[path]["import"]["artifacts"], after[path]
        report["assets"] = {
            path: {
                "source_sha256": after[path]["sha256"],
                "importer": after[path]["import"]["importer"],
                "artifacts": [
                    {"kind": artifact["kind"], "sha256": artifact["sha256"], "bytes": artifact["bytes"]}
                    for artifact in after[path]["import"]["artifacts"]
                ],
            }
            for path in before
        }
        everything = "\n".join(client.transcript)
        assert str(workspace) not in everything, "an answer leaked the workspace path"
        assert ".godot/imported" not in everything, "an answer leaked an engine cache path"


def verify_with_godot(godot: Path, project: Path, scratch: Path) -> dict:
    script = scratch / "verify_settings.gd"
    script.write_text(VERIFY, encoding="utf-8")
    home = scratch / "home"
    home.mkdir(exist_ok=True)
    completed = subprocess.run(
        [str(godot), "--headless", "--path", str(project), "--script", str(script)],
        capture_output=True, text=True, timeout=120, env=private_godot_environment(home),
    )
    line = next((row for row in completed.stdout.splitlines() if row.startswith("VERIFY ")), None)
    assert line is not None, (completed.returncode, completed.stdout, completed.stderr)
    facts = json.loads(line[len("VERIFY "):])
    assert facts["main_scene"] == "res://scenes/main.tscn", facts
    assert facts["autoload"] == "*res://scripts/game.gd", facts
    assert facts["name"] == "M2 Renamed", facts
    assert facts["jump_events"] == 2, facts
    assert facts["jump_first_physical_key"] == facts["space"], facts
    return facts


async def restart_and_reread(arguments: argparse.Namespace, project: Path, report: dict) -> None:
    async with connect(Server(binary=arguments.binary, project=project, profile="full",
                              godot=arguments.godot)) as client:
        settings = (await client.call("project.settings.get")).require_ok()
        assert settings["main_scene"] == "res://scenes/main.tscn", settings
        assert settings["autoloads"] == [{"name": "Game", "path": "res://scripts/game.gd", "singleton": True}]
        assert settings["input_actions"][0]["events"][0]["key"] == "Space", settings
        assert settings["workspace"]["coherent"] is True, settings
        listed = (await client.call("project.files.list", {"limit": 1000})).require_ok()
        paths = {entry["path"] for entry in listed["files"]}
        for expected in ("scripts/player.gd", "scenes/main.tscn", "art/icon.png", "audio/blip.wav"):
            assert expected in paths, sorted(paths)
        report["restart"] = {"revision": settings["workspace"]["revision"], "files": len(paths)}


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    report: dict = {}
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m2-") as scratch:
        scratch_path = Path(scratch).resolve()
        workspace = scratch_path / "workspace"
        workspace.mkdir()
        await bootstrap_and_author(arguments, workspace, report)
        project = workspace / "game"
        report["engine_readback"] = verify_with_godot(arguments.godot, project, scratch_path)
        await restart_and_reread(arguments, project, report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
