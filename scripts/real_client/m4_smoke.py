"""M4 real-entry smoke: the godot_mcp editor add-on in a real Godot editor.

Usage:
    python3 -I -B scripts/real_client/m4_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK while a
real headless Godot editor runs the installed add-on:

1. `editor.addon.install` writes the bundled add-on and enables it; a scene is
   created headless; the editor is started and its plugin connects.
2. `editor.scene.open`, `editor.scene.tree.get` and `editor.status.get`: reads
   add no undo history and mark nothing unsaved.
3. `editor.selection.set`, then one `scene.batch.apply` that the server routes
   to the editor because the scene is open there: one undoable action, saved;
   the live digest after it equals the batch's digest, `editor.undo` returns to
   the pre-state digest and `editor.redo` to the post-state digest.
4. An unsaved editor change refuses a batch (never overwritten); a headless
   `file.patch` of the open scene refuses; `editor.types.get` reads the exact
   build's class metadata; `editor.addon.reload` reconnects with a new session.
5. Off the editor: the dispatcher refuses work requested from another thread,
   and the add-on's tagged-value codec matches the shared golden vectors.
6. A residue probe plugin enables, reloads and disables the add-on inside a
   second real editor and reports what each disable left behind.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect, private_godot_environment  # noqa: E402

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
BRIDGE = HERE.parents[1] / "crates" / "godot-mcp-bridge"
ADDON = BRIDGE / "addon" / "godot_mcp"
CODEC_GOLDENS = BRIDGE / "tests" / "goldens" / "variant-codec" / "v1.json"
TYPE_GOLDENS = BRIDGE / "tests" / "goldens" / "type-catalog"
GOLDEN_CLASSES = ["Node", "Node2D", "CharacterBody2D", "Area2D", "Label", "Camera3D"]

PROJECT = 'config_version=5\n\n[application]\n\nconfig/name="m4"\n'

HERO = """class_name Hero
extends CharacterBody2D

signal scored(points: int)

@export var speed := 200.0
"""


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def editor_process(godot: Path, project: Path, home: Path, log: Path) -> subprocess.Popen:
    home.mkdir(parents=True, exist_ok=True)
    return subprocess.Popen(
        [str(godot), "--headless", "--editor", "--path", str(project)],
        stdout=log.open("w", encoding="utf-8"),
        stderr=subprocess.STDOUT,
        env=private_godot_environment(home),
    )


async def wait_status(client, predicate, what: str, timeout: float = 120.0) -> dict:
    deadline = time.monotonic() + timeout
    while True:
        status = (await client.call("editor.status.get")).require_ok()
        if predicate(status):
            return status
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}: {status}")
        await asyncio.sleep(0.25)


async def main_flow(arguments: argparse.Namespace, scratch: Path, report: dict) -> None:
    project = scratch / "game"
    project.mkdir()
    (project / "project.godot").write_text(PROJECT, encoding="utf-8")
    async with connect(Server(binary=arguments.binary, project=project, profile="full",
                              godot=arguments.godot)) as client:
        status = (await client.call("editor.addon.status")).require_ok()
        assert status["installed"] is False and status["install_allowed"] is True, status
        (await client.call("editor.addon.install", {"enable": True, "idempotencyId": "m4-install"})).require_ok()
        created = (await client.call("scene.create", {
            "path": "scenes/main.tscn", "rootType": "Node2D", "rootName": "Main",
            "operations": [{"op": "add_node", "parent": ".", "name": "Player", "type": "Node2D"}],
            "idempotencyId": "m4-scene",
        })).require_ok()
        assert created["backend"] == "headless", created
        (await client.call("script.create", {
            "path": "scripts/hero.gd", "content": HERO, "idempotencyId": "m4-hero"})).require_ok()

        editor_log = scratch / "editor.log"
        editor = editor_process(arguments.godot, project, scratch / "home", editor_log)
        try:
            try:
                connected = await wait_status(client, lambda s: s["connected"], "the plugin to connect")
            except AssertionError as error:
                exited = editor.poll()
                tail = editor_log.read_text(encoding="utf-8", errors="replace")[-3000:]
                raise AssertionError(f"{error}; editor exit={exited}; editor log:\n{tail}") from None
            report["plugin"] = {key: connected[key] for key in ("plugin_version", "godot_version")}

            opened = (await client.call("editor.scene.open", {
                "path": "scenes/main.tscn", "idempotencyId": "m4-open"})).require_ok()
            assert opened["edited_scene"] == "res://scenes/main.tscn", opened
            before = (await client.call("editor.scene.tree.get")).require_ok()
            state = (await client.call("editor.status.get")).require_ok()["editor"]
            assert state["history"]["has_undo"] is False, state
            assert all(scene["unsaved"] in (False, None) for scene in state["open_scenes"]), state

            selected = (await client.call("editor.selection.set", {
                "nodes": ["Player"], "idempotencyId": "m4-select"})).require_ok()
            assert selected["selection"] == ["Player"], selected

            disk = (project / "scenes" / "main.tscn").read_bytes()
            batch = (await client.call("scene.batch.apply", {
                "path": "scenes/main.tscn", "expectedSha256": sha256(disk),
                "operations": [
                    {"op": "set_property", "node": "Player", "property": "position",
                     "value": {"type": "vector2", "value": {"x": 12.0, "y": 34.0}}},
                    {"op": "add_group", "node": "Player", "group": "players"},
                ],
                "idempotencyId": "m4-batch",
            })).require_ok()
            assert batch["backend"] == "editor", batch
            saved = (project / "scenes" / "main.tscn").read_bytes()
            assert batch["after_sha256"] == sha256(saved), batch
            assert b"position = Vector2(12, 34)" in saved and b"players" in saved, saved
            after = (await client.call("editor.scene.tree.get")).require_ok()
            assert after["digest"] == batch["editor"]["digest"], (after["digest"], batch["editor"])
            assert after["digest"] != before["digest"]

            undone = (await client.call("editor.undo", {"idempotencyId": "m4-undo"})).require_ok()
            assert undone["digest"] == before["digest"], "undo returns to the semantic pre-state"
            # The undo went through the editor's own history, so the editor
            # itself now knows the scene differs from its saved file.
            undone_state = (await client.call("editor.status.get")).require_ok()["editor"]
            main_scene = next(s for s in undone_state["open_scenes"] if s["path"] == "res://scenes/main.tscn")
            assert main_scene["unsaved"] is True, undone_state
            redone = (await client.call("editor.redo", {"idempotencyId": "m4-redo"})).require_ok()
            assert redone["digest"] == after["digest"], "redo returns to the semantic post-state"

            # An unsaved editor change is never overwritten.
            (await client.call("editor.undo", {"idempotencyId": "m4-undo-unsaved"})).require_ok()
            current = (project / "scenes" / "main.tscn").read_bytes()
            refused = await client.call("scene.batch.apply", {
                "path": "scenes/main.tscn", "expectedSha256": sha256(current),
                "operations": [{"op": "add_group", "node": "Player", "group": "refused"}],
                "idempotencyId": "m4-divergent",
            })
            assert refused.is_error and refused.payload["code"] == "conflict", refused.raw_text
            assert "unsaved" in refused.raw_text or "holds changes" in refused.raw_text, refused.raw_text
            assert (project / "scenes" / "main.tscn").read_bytes() == current
            (await client.call("editor.redo", {"idempotencyId": "m4-redo-clean"})).require_ok()
            # The saved file reproduces the post-state when the editor reloads it,
            # and the reload keeps the selection.
            selection_before = (await client.call("editor.status.get")).require_ok()["editor"]["selection"]
            assert selection_before == ["Player"], selection_before
            reloaded_scene = (await client.call("editor.scene.open", {
                "path": "scenes/main.tscn", "reload": True, "idempotencyId": "m4-reload-scene"})).require_ok()
            assert reloaded_scene["reloaded"] is True, reloaded_scene
            selection_after = (await client.call("editor.status.get")).require_ok()["editor"]["selection"]
            assert selection_after == selection_before, (selection_before, selection_after)
            from_file = (await client.call("editor.scene.tree.get")).require_ok()
            assert from_file["digest"] == after["digest"], "the saved file reloads to the post-state"

            held = await client.call("file.patch", {
                "path": "scenes/main.tscn", "expectedSha256": sha256(current),
                "change": {"content": current.decode("utf-8") + "; edited around the editor\n"},
                "idempotencyId": "m4-held",
            })
            assert held.is_error and held.payload["code"] == "conflict", held.raw_text
            assert (project / "scenes" / "main.tscn").read_bytes() == current

            hero = (await client.call("editor.types.get", {"class": "Hero"})).require_ok()
            assert hero["script"] == "res://scripts/hero.gd", hero
            assert hero["inherits"][0] == "CharacterBody2D", hero["inherits"]
            assert any(p["name"] == "speed" and p["type"] == "float" for p in hero["properties"]), hero
            assert [signal["name"] for signal in hero["signals"]] == ["scored"], hero["signals"]
            types = (await client.call("editor.types.get", {"class": "Node2D"})).require_ok()
            assert any(p["name"] == "position" for p in types["properties"]), types["properties"][:5]
            assert types["inherits"][:2] == ["CanvasItem", "Node"], types["inherits"]
            catalogue = {}
            for name in GOLDEN_CLASSES:
                described = (await client.call("editor.types.get", {"class": name})).require_ok()
                catalogue[name] = {
                    "inherits": described["inherits"],
                    "properties": sorted(f'{p["name"]}:{p["type"]}' for p in described["properties"]),
                    "signals": sorted(signal["name"] for signal in described["signals"]),
                }
            version = connected["godot_version"].split("-")[0]
            golden = TYPE_GOLDENS / f"{version}.json"
            if arguments.write_type_goldens:
                golden.parent.mkdir(parents=True, exist_ok=True)
                golden.write_text(json.dumps(catalogue, indent=1, sort_keys=True) + "\n", encoding="utf-8")
            expected = json.loads(golden.read_text(encoding="utf-8"))
            assert catalogue == expected, f"the {version} type catalogue drifted from {golden.name}"
            report["type_catalogue"] = {"golden": golden.name, "classes": len(catalogue)}

            opened_before = (await client.call("editor.status.get")).require_ok()["sessions_opened"]
            (await client.call("editor.addon.reload", {"idempotencyId": "m4-reload"})).require_ok()
            reloaded = await wait_status(
                client,
                lambda s: s["connected"] and s["sessions_opened"] > opened_before,
                "the reloaded plugin to reconnect",
            )
            # Every history step kept the editor's own bookkeeping consistent.
            editor_log = (scratch / "editor.log").read_text(encoding="utf-8", errors="replace")
            inconsistent = [row for row in editor_log.splitlines() if "Inconsistent" in row and "history" in row]
            assert not inconsistent, inconsistent
            report["main"] = {
                "batch_backend": batch["backend"],
                "undo_restores_pre_state": undone["digest"] == before["digest"],
                "redo_restores_post_state": redone["digest"] == after["digest"],
                "reload_restores_post_state": from_file["digest"] == after["digest"],
                "project_class": hero["class"],
                "divergent_buffer_refused": refused.payload["code"],
                "headless_write_to_open_scene": held.payload["code"],
                "sessions_after_reload": reloaded["sessions_opened"],
                "node2d_properties": len(types["properties"]),
            }
            assert str(project) not in "\n".join(client.transcript), "an answer leaked the project path"
        except BaseException:
            # The scratch directory is removed on exit, so the editor's own log
            # is the only record of why it went away; keep its tail.
            editor.kill()
            editor.wait(timeout=30)
            log = (scratch / "editor.log").read_text(encoding="utf-8", errors="replace")
            print(f"editor exit={editor.returncode}; editor log tail:\n{log[-6000:]}", file=sys.stderr)
            raise
        finally:
            editor.kill()
            editor.wait(timeout=30)


def main_thread_probe(godot: Path, scratch: Path, report: dict) -> None:
    project = scratch / "probe-thread"
    shutil.copytree(ADDON, project / "addons" / "godot_mcp")
    (project / "project.godot").write_text(PROJECT, encoding="utf-8")
    shutil.copy(FIXTURES / "main_thread_probe.gd", project / "main_thread_probe.gd")
    home = scratch / "home-thread"
    home.mkdir()
    completed = subprocess.run(
        [str(godot), "--headless", "--path", str(project), "--script", "res://main_thread_probe.gd"],
        capture_output=True, text=True, timeout=180, env=private_godot_environment(home),
    )
    line = next((row for row in completed.stdout.splitlines() if row.startswith("MAIN_THREAD_PROBE ")), None)
    assert line is not None, (completed.returncode, completed.stdout[-2000:], completed.stderr[-2000:])
    probe = json.loads(line[len("MAIN_THREAD_PROBE "):])
    assert probe["off_main"]["ok"] is False, probe
    assert "main thread" in probe["off_main"]["message"], probe
    assert probe["off_main"]["mutation_status"] == "not_started", probe
    assert probe["off_main_handler_calls"] == 0, probe
    assert probe["main"]["ok"] is True and probe["main_handler_calls"] == 1, probe
    report["main_thread"] = {"off_main_refused": True, "main_ran": True}


def variant_codec_probe(godot: Path, scratch: Path, report: dict) -> None:
    project = scratch / "probe-codec"
    shutil.copytree(ADDON, project / "addons" / "godot_mcp")
    (project / "project.godot").write_text(PROJECT, encoding="utf-8")
    shutil.copy(FIXTURES / "variant_codec_probe.gd", project / "variant_codec_probe.gd")
    shutil.copy(CODEC_GOLDENS, project / "variant-codec-v1.json")
    (project / "codec").mkdir()
    (project / "codec" / "sample.tres").write_text(
        '[gd_resource type="Resource" format=3]\n\n[resource]\n', encoding="utf-8")
    home = scratch / "home-codec"
    home.mkdir()
    completed = subprocess.run(
        [str(godot), "--headless", "--path", str(project), "--script", "res://variant_codec_probe.gd"],
        capture_output=True, text=True, timeout=180, env=private_godot_environment(home),
    )
    line = next((row for row in completed.stdout.splitlines() if row.startswith("VARIANT_CODEC_PROBE ")), None)
    assert line is not None, (completed.returncode, completed.stdout[-2000:], completed.stderr[-2000:])
    probe = json.loads(line[len("VARIANT_CODEC_PROBE "):])
    assert probe["failures"] == [], probe
    report["variant_codec"] = {"vectors": probe["vectors"], "refusals": probe["refusals"], "failures": 0}


async def residue_probe(arguments: argparse.Namespace, scratch: Path, report: dict) -> None:
    project = scratch / "probe-residue"
    project.mkdir()
    (project / "project.godot").write_text(
        PROJECT + '\n[editor_plugins]\n\nenabled=PackedStringArray("res://addons/godot_mcp_probe/plugin.cfg")\n',
        encoding="utf-8",
    )
    shutil.copytree(FIXTURES / "residue_probe", project / "addons" / "godot_mcp_probe")
    async with connect(Server(binary=arguments.binary, project=project, profile="full",
                              godot=arguments.godot)) as client:
        (await client.call("editor.addon.install", {"idempotencyId": "probe-install"})).require_ok()
        log = scratch / "residue-editor.log"
        editor = editor_process(arguments.godot, project, scratch / "home-residue", log)
        try:
            editor.wait(timeout=300)
        except subprocess.TimeoutExpired:
            editor.kill()
            raise AssertionError("the residue probe did not finish: " + log.read_text(encoding="utf-8")[-3000:])
        text = log.read_text(encoding="utf-8")
        line = next((row for row in text.splitlines() if row.startswith("RESIDUE_PROBE ")), None)
        assert line is not None, text[-3000:]
        probe = json.loads(line[len("RESIDUE_PROBE "):])
        assert "timed_out_at_step" not in probe, probe
        baseline = probe["baseline"]
        for enabled in ("enabled_once", "enabled_twice"):
            assert probe[enabled]["bridge_nodes"] == 1 and probe[enabled]["dock_nodes"] == 1, probe
        for disabled in ("disabled_once", "disabled_twice"):
            after = probe[disabled]
            assert after["enabled"] is False, probe
            assert after["bridge_nodes"] == 0 and after["dock_nodes"] == 0, probe
            assert after["reloader_nodes"] == 0, probe
            assert after["nodes"] == baseline["nodes"], (disabled, baseline, after)
            assert after["orphans"] == baseline["orphans"], (disabled, baseline, after)
        status = (await client.call("editor.status.get")).require_ok()
        assert status["sessions_opened"] == 2 and status["connected"] is False, status
        report["residue"] = {
            "baseline": baseline,
            "disabled_once": probe["disabled_once"],
            "disabled_twice": probe["disabled_twice"],
            "sessions_opened": status["sessions_opened"],
        }


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    parser.add_argument("--write-type-goldens", action="store_true",
                        help="write the type catalogue golden for this build instead of only comparing")
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    report: dict = {}
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m4-") as scratch:
        scratch_path = Path(scratch).resolve()
        await main_flow(arguments, scratch_path, report)
        main_thread_probe(arguments.godot, scratch_path, report)
        variant_codec_probe(arguments.godot, scratch_path, report)
        await residue_probe(arguments, scratch_path, report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
