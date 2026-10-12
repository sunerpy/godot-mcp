"""Complete-game real-entry acceptance: an empty workspace becomes a tested, exported game
through MCP alone.

Usage:
    python3 -I -B scripts/real_client/e2e_complete.py --binary <godot-mcp> --godot <Godot editor> \
        --gut <GUT release archive for this Godot minor>

Drives the built `godot-mcp` binary through the official MCP Python SDK against a real
Godot editor build, with nothing written to the project outside MCP:

 1. `serve --workspace` over an empty directory; `project.create` and `project.bind`.
 2. The GUT add-on the build's minor version needs, from the release archive given, is
    committed into the project file by file (`file.create`, `asset.put`); the archive's
    generated `.import` and `.uid` files are left to the engine.
 3. A player script and a GUT test are created, the main scene, a `jump` input action and
    the ETC2/ASTC texture imports a macOS export needs are set as typed settings, and a
    sprite is uploaded.
 4. The project is imported by a real Godot run, and the script is engine-checked.
 5. The main scene is authored: a sprite with the uploaded texture and the script, and a
    score label.
 6. A managed game session runs it in a window: the tree and properties are read, a `jump`
    tap moves the player and changes the label, a screenshot is captured and read back
    through `resources/read` to its SHA-256, and the session stops with its boundary empty.
 7. `project.test` runs the committed GUT suite: passed, no failure.
 8. The host's own desktop preset — Linux on Linux, Windows on Windows, macOS on a Mac — is
    written and exported through the build's exact templates into a verified artifact,
    and the exported build runs once headless and exits cleanly: on a Mac, the executable
    of the app bundle the export packed into one zip.
 9. A fresh server bound to the project names the same project identity, describes the
    same file inventory and settings by digest as the last answer before it, coherently,
    reads the same settings, scene, script and asset back, and still serves the export's
    artifact.

Export templates are looked up where Godot looks for them (`XDG_DATA_HOME` on Linux, `HOME`
on macOS, `APPDATA` on Windows). The game window needs a display (`DISPLAY`, for example
an Xvfb server, on Linux).
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import struct
import sys
import tempfile
import time
import zipfile
import zlib
from pathlib import Path, PurePosixPath

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect  # noqa: E402

CHUNK = 256 * 1024
# Importing a project that carries GUT's fonts takes longer than the default
# 30 s operation ceiling on a busy host; the server's ceiling is raised to this
# and each engine run asks for it.
DEADLINE_MS = 300_000

PLAYER = """extends Sprite2D

signal scored(score: int)

var score := 0


func next_score(value: int) -> int:
\treturn value + 1


func _process(_delta: float) -> void:
\tif Input.is_action_just_pressed("jump"):
\t\tscore = next_score(score)
\t\tposition.x += 16.0
\t\tvar label := get_node_or_null("../Hud/Score") as Label
\t\tif label:
\t\t\tlabel.text = "Score: %d" % score
\t\tscored.emit(score)
\t\tprint("scored %d" % score)
"""

PLAYER_TEST = """extends GutTest


func test_next_score_adds_one() -> void:
\tvar player: Sprite2D = load("res://scripts/player.gd").new()
\tassert_eq(player.next_score(1), 2)
\tplayer.free()


func test_a_new_player_has_no_score() -> void:
\tvar player: Sprite2D = load("res://scripts/player.gd").new()
\tassert_eq(player.score, 0)
\tplayer.free()
"""

PRESETS = """[preset.0]

name="Linux"
platform="Linux"
runnable=true
export_filter="all_resources"
export_path="build/game.x86_64"

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=true
binary_format/architecture="x86_64"

[preset.1]

name="Windows"
platform="Windows Desktop"
runnable=false
export_filter="all_resources"
export_path="build/game.exe"

[preset.1.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=true
binary_format/architecture="x86_64"

[preset.2]

name="Mac"
platform="macOS"
runnable=true
export_filter="all_resources"
export_path="build/game.app"

[preset.2.options]

custom_template/debug=""
custom_template/release=""
binary_format/architecture="universal"
application/bundle_identifier="org.godotmcp.e2e"
codesign/codesign=1
"""

# The desktop preset of this host's own platform, whose export this host runs once, and
# the file that run starts: the export itself, or the executable inside the app bundle
# a macOS export packs into one zip, named after the project, `E2E Game`.
HOST_PRESET, HOST_EXECUTABLE = {
    "linux": ("Linux", "game.x86_64"),
    "win32": ("Windows", "game.exe"),
    "darwin": ("Mac", "E2E Game.app/Contents/MacOS/E2E Game"),
}[sys.platform]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def sprite_png(size: int = 32) -> bytes:
    """A small opaque PNG: a filled square with a darker border."""
    rows = bytearray()
    for y in range(size):
        rows.append(0)
        for x in range(size):
            edge = x in (0, size - 1) or y in (0, size - 1)
            rows.extend((20, 60, 140) if edge else (80, 170, 250))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(bytes(rows))) + chunk(b"IEND", b""))


def vector2(x: float, y: float) -> dict:
    return {"type": "vector2", "value": {"x": float(x), "y": float(y)}}


def gut_files(archive: Path) -> list[tuple[str, bytes]]:
    """The add-on's files from a GUT release archive, project-relative, without the
    engine-generated `.import` and `.uid` sidecars."""
    files = []
    with zipfile.ZipFile(archive) as bundle:
        for entry in bundle.infolist():
            if entry.is_dir():
                continue
            parts = PurePosixPath(entry.filename).parts
            if "addons" not in parts:
                continue
            relative = PurePosixPath(*parts[parts.index("addons"):])
            if relative.parts[:2] != ("addons", "gut") or relative.suffix in (".import", ".uid"):
                continue
            files.append((str(relative), bundle.read(entry)))
    files.sort()
    return files


async def put_file(client, path: str, data: bytes, key: str) -> None:
    """Commit one file through MCP: text through `file.create`, binary through `asset.put`."""
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        text = None
    if text is not None and "\x00" not in text:
        (await client.call("file.create", {"path": path, "content": text, "idempotencyId": key})).require_ok()
        return
    if len(data) <= CHUNK:
        (await client.call("asset.put", {
            "path": path, "contentBase64": b64(data), "sha256": sha256(data), "idempotencyId": key,
        })).require_ok()
        return
    upload = None
    last = None
    for index, offset in enumerate(range(0, len(data), CHUNK)):
        arguments = {
            "path": path, "contentBase64": b64(data[offset:offset + CHUNK]), "sha256": sha256(data),
            "totalBytes": len(data), "idempotencyId": f"{key}-{index}",
        }
        if upload is not None:
            arguments["upload"] = {"id": upload, "offset": offset}
        last = (await client.call("asset.put", arguments)).require_ok()
        upload = last.get("upload_id", upload)
    assert last is not None and last["complete"] is True, last


async def until(predicate, what: str, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    while not await predicate():
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        await asyncio.sleep(0.1)


async def read_range(client, uri: str, start: int, end: int) -> bytes:
    from mcp import types

    request = types.ClientRequest(types.ReadResourceRequest(
        params=types.ReadResourceRequestParams(
            uri=uri, _meta={"io.godot-mcp/artifactRange": {"start": start, "endInclusive": end}},
        ),
    ))
    result = await client.session.send_request(request, types.ReadResourceResult)
    return base64.b64decode(result.contents[0].blob)


async def read_artifact(client, uri: str, total: int, chunk: int = 4 * 1024 * 1024) -> bytes:
    data = bytearray()
    while len(data) < total:
        data.extend(await read_range(client, uri, len(data), min(total, len(data) + chunk) - 1))
    return bytes(data)


async def author(client, gut: Path, report: dict) -> dict:
    """Steps 1-5: an empty workspace becomes an imported, authored project."""
    created = (await client.call("project.create", {
        "path": "game", "name": "E2E Game", "renderer": "gl_compatibility",
        "idempotencyId": "e2e-create",
    })).require_ok()
    assert created["path"] == "game", created
    bound = (await client.call("project.bind", {"path": "game"})).require_ok()
    assert bound["bound"] is True, bound

    addon = gut_files(gut)
    for index, (path, data) in enumerate(addon):
        await put_file(client, path, data, f"e2e-gut-{index}")
    report["gut"] = {"archive": gut.name, "archive_sha256": sha256(gut.read_bytes()), "files": len(addon)}

    script = (await client.call("script.create", {
        "path": "scripts/player.gd", "content": PLAYER, "idempotencyId": "e2e-player",
    })).require_ok()
    (await client.call("script.create", {
        "path": "test/test_player.gd", "content": PLAYER_TEST, "idempotencyId": "e2e-test",
    })).require_ok()
    png = sprite_png()
    put = (await client.call("asset.put", {
        "path": "art/player.png", "contentBase64": b64(png), "sha256": sha256(png),
        "idempotencyId": "e2e-sprite",
    })).require_ok()
    assert put["conclusion"] == "proven_post_state", put

    imported = (await client.call("project.import.run", {
        "assets": ["art/player.png"], "deadline_ms": DEADLINE_MS, "idempotencyId": "e2e-import",
    })).require_ok()
    assert imported["classification"] == "passed", json.dumps(imported, indent=2)[:4000]
    sprite = imported["assets"][0]
    assert sprite["import"]["state"] == "imported" and sprite["sha256"] == sha256(png), sprite
    checked = (await client.call("script.check", {"script": "scripts/player.gd"})).require_ok()
    assert checked["classification"] == "passed", checked

    scene = (await client.call("scene.create", {
        "path": "scenes/main.tscn", "rootType": "Node2D", "rootName": "Main",
        "operations": [
            {"op": "add_node", "parent": ".", "name": "Player", "type": "Sprite2D"},
            {"op": "attach_script", "node": "Player", "script": "res://scripts/player.gd"},
            {"op": "set_property", "node": "Player", "property": "position", "value": vector2(64, 64)},
            {"op": "set_property", "node": "Player", "property": "texture", "value": {
                "type": "resource", "value": {"resource": "res://art/player.png",
                                              "resource_type": "Texture2D"}}},
            {"op": "add_node", "parent": ".", "name": "Hud", "type": "CanvasLayer"},
            {"op": "add_node", "parent": "Hud", "name": "Score", "type": "Label"},
            {"op": "set_property", "node": "Hud/Score", "property": "text",
             "value": {"type": "string", "value": "Score: 0"}},
        ],
        "idempotencyId": "e2e-scene",
    })).require_ok()
    assert scene["conclusion"] == "proven_post_state", scene

    settings = (await client.call("project.settings.get")).require_ok()
    patched = (await client.call("project.settings.patch", {
        "expectedSha256": settings["sha256"],
        "operations": [
            {"op": "set_main_scene", "scene": "res://scenes/main.tscn"},
            {"op": "set_input_action", "name": "jump", "events": [{"type": "key", "key": "Space"}]},
            {"op": "set_value", "setting": "rendering/textures/vram_compression/import_etc2_astc",
             "value": True},
        ],
        "idempotencyId": "e2e-settings",
    })).require_ok()
    report["authored"] = {
        "script_sha256": script["after_sha256"],
        "scene_sha256": scene["after_sha256"],
        "sprite_sha256": sha256(png),
        "settings_revision": patched["workspace"]["revision"],
        "engine_check": checked["classification"],
    }
    return {"script_sha256": script["after_sha256"], "scene_sha256": scene["after_sha256"],
            "sprite_sha256": sha256(png)}


async def play(client, report: dict) -> None:
    """Step 6: run, inspect, drive, capture and stop the game."""
    started = (await client.call("project.run.start", {
        "idempotencyId": "e2e-run", "display": "window",
        "resolution": {"width": 320, "height": 240},
    })).require_ok()
    session = started["session"]["session"]
    assert started["session"]["state"] == "running" and started["session"]["probe"]["connected"], started

    tree = (await client.call("runtime.tree.get", {"session": session, "depth": 3})).require_ok()
    paths = {node["path"] for node in tree["nodes"]}
    assert {"/root/Main/Player", "/root/Main/Hud/Score"} <= paths, sorted(paths)

    async def player(name: str):
        read = (await client.call("runtime.properties.get", {
            "session": session, "node": "Player", "properties": [name]})).require_ok()
        return read["properties"][name]["value"]

    before = await player("position")
    assert float(before["x"]) == 64.0, before
    sent = (await client.call("input.action.send", {
        "session": session, "idempotencyId": "e2e-jump", "action": "jump", "state": "tap",
        "holdMs": 100})).require_ok()
    assert sent["mutation_status"] == "committed", sent

    async def scored() -> bool:
        return await player("score") == 1

    await until(scored, "the jump to score")
    after = await player("position")
    assert float(after["x"]) == 80.0, after

    async def labelled() -> bool:
        ui = (await client.call("runtime.ui.get", {"session": session})).require_ok()
        return "Score: 1" in {entry.get("text") for entry in ui["controls"]}

    await until(labelled, "the label to show the score")

    shot = (await client.call("runtime.screenshot.capture", {
        "session": session, "idempotencyId": "e2e-shot"})).require_ok()["artifact"]
    assert (shot["width"], shot["height"]) == (320, 240), shot
    png = await read_artifact(client, shot["uri"], shot["byte_length"])
    assert png[:8] == b"\x89PNG\r\n\x1a\n" and sha256(png) == shot["sha256"], len(png)

    stopped = (await client.call("project.run.stop", {
        "session": session, "idempotencyId": "e2e-stop"})).require_ok()
    assert stopped["state"] == "exited" and stopped["exit"]["boundary_empty"] is True, stopped
    logs = (await client.call("project.run.logs", {"session": session})).require_ok()
    assert "scored 1" in logs["text"], logs["text"][-800:]
    report["play"] = {
        "boundary": {"kind": started["execution"]["boundary_kind"],
                     "strength": started["execution"]["boundary_strength"]},
        "moved": [float(before["x"]), float(after["x"])],
        "screenshot_sha256": shot["sha256"],
        "stopped": stopped["state"],
    }


async def test_and_export(client, report: dict) -> dict:
    """Steps 7-8: the committed GUT suite, then a verified desktop export."""
    tested = (await client.call("project.test", {
        "selection": ["res://test"], "deadline_ms": DEADLINE_MS})).require_ok()
    summary = tested.get("summary") or {}
    assert tested["classification"] == "passed", json.dumps(tested, indent=2)[:4000]
    assert summary.get("failed") == 0 and summary.get("passed", 0) >= 2, summary
    report["test"] = {"framework": tested.get("framework"), "summary": summary}

    (await client.call("file.create", {
        "path": "export_presets.cfg", "content": PRESETS, "idempotencyId": "e2e-presets",
    })).require_ok()
    listed = (await client.call("export.presets.list")).require_ok()
    host = next(preset for preset in listed["presets"] if preset["name"] == HOST_PRESET)["template"]
    assert host["installed"] is True, listed
    started = (await client.call("project.export", {
        "preset": HOST_PRESET, "smoke": True, "idempotencyId": "e2e-export"})).require_ok()
    export = started["export"]["session"]
    deadline = time.monotonic() + 600
    while True:
        status = (await client.call("project.export.status", {"export": export})).require_ok()
        if status["state"] not in ("running", "starting"):
            break
        assert time.monotonic() < deadline, status
        await asyncio.sleep(0.5)
    assert status["export"]["outcome"] == "succeeded", status
    output = status["export"]["outputs"][0]
    data = await read_artifact(client, output["artifact"], output["bytes"])
    assert sha256(data) == output["sha256"], (sha256(data), output["sha256"])
    smoke = status["export"]["smoke"]
    assert smoke["ran"] is True and smoke["passed"] is True, smoke
    assert smoke["termination"] == "exited_clean" and smoke["executable"] == HOST_EXECUTABLE, smoke
    report["export"] = {"preset": HOST_PRESET, "template": host["label"], "file": host["file"],
                        "output": output["name"], "bytes": output["bytes"],
                        "sha256": output["sha256"], "smoke": smoke}
    # The last answer before the restart: what the restarted server must agree with.
    last = (await client.call("project.info.get")).require_ok()
    assert last["workspace"]["coherent"] is True, last["workspace"]
    return {"export": export, "artifact": output["artifact"], "sha256": output["sha256"],
            "project": last["project"], "workspace": last["workspace"]}


async def verify_restart(client, authored: dict, exported: dict, report: dict) -> None:
    """Step 9: a fresh server reads everything back."""
    info = (await client.call("project.info.get")).require_ok()
    assert info["project"]["object_id"] == exported["project"]["object_id"], (info["project"], exported["project"])
    workspace = info["workspace"]
    assert workspace["coherent"] is True and workspace["stale_reason"] is None, workspace
    for key in ("files", "inventory_digest", "settings_digest"):
        assert workspace[key] == exported["workspace"][key], (key, workspace, exported["workspace"])
    settings = (await client.call("project.settings.get")).require_ok()
    assert settings["main_scene"] == "res://scenes/main.tscn", settings
    assert any(action["name"] == "jump" for action in settings["input_actions"]), settings
    script = (await client.call("file.get", {"path": "scripts/player.gd"})).require_ok()
    assert script["sha256"] == authored["script_sha256"], script
    tree = (await client.call("scene.tree.get", {"scene": "scenes/main.tscn"})).require_ok()["data"]
    assert tree["sha256"] == authored["scene_sha256"], tree
    nodes = {node["path"] for node in tree["nodes"]}
    assert {"Player", "Hud", "Hud/Score"} <= nodes or {".", "Player", "Hud/Score"} <= nodes, sorted(nodes)
    sprite = (await client.call("asset.status", {"path": "art/player.png"})).require_ok()
    assert sprite["sha256"] == authored["sprite_sha256"] and sprite["import"]["state"] == "imported", sprite
    status = (await client.call("project.export.status", {"export": exported["export"]})).require_ok()
    assert status["export"]["outputs"][0]["artifact"] == exported["artifact"], status
    head = await read_range(client, exported["artifact"], 0, 4095)
    assert len(head) == 4096, len(head)
    report["restart"] = {"project_object_id_unchanged": True, "coherent": True,
                         "inventory_digest": workspace["inventory_digest"], "files": workspace["files"],
                         "settings_digest": workspace["settings_digest"],
                         "revision_before": exported["workspace"]["revision"],
                         "revision_after": settings["workspace"]["revision"], "scene_nodes": len(nodes),
                         "export_still_served": True}


async def flow(arguments: argparse.Namespace, scratch: Path, report: dict) -> None:
    workspace = scratch / "workspace"
    workspace.mkdir()
    limits = {"limits.deadline_ms": DEADLINE_MS}
    async with connect(Server(binary=arguments.binary, workspace=workspace, profile="full",
                              godot=arguments.godot, extra_config=limits)) as client:
        authored = await author(client, arguments.gut, report)
        await play(client, report)
        exported = await test_and_export(client, report)
    async with connect(Server(binary=arguments.binary, project=workspace / "game", profile="full",
                              godot=arguments.godot, extra_config=limits)) as client:
        await verify_restart(client, authored, exported, report)


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    parser.add_argument("--gut", type=Path, required=True)
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    arguments.gut = arguments.gut.resolve()
    report: dict = {"godot": arguments.godot.name}
    # Windows deletes a file only once its last handle closes, so a tree the
    # engine wrote until a moment ago may refuse a prompt removal.
    with tempfile.TemporaryDirectory(prefix="godot-mcp-e2e-", ignore_cleanup_errors=sys.platform == "win32") as scratch:
        await flow(arguments, Path(scratch).resolve(), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
