"""M3 real-entry smoke: semantic scene and resource authoring through real Godot.

Usage:
    python3 -I -B scripts/real_client/m3_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK:

1. a text resource is created; a 2D scene is created with nodes, an attached
   script, typed properties, a collision sub-resource, a reference to that
   resource, an instanced child scene, a signal connection and persistent
   groups; a 3D scene with a camera, a mesh sub-resource and a light; the
   resource is then edited by a batch and keeps its UID;
2. the 2D scene is edited by one atomic batch (rename, move, property, group);
3. a stale hash and an external change made on disk both refuse a batch and
   leave the file exactly as it was;
4. node handles from a scene tree read follow their node through a rename in
   the same batch, never name a replacement created at the node's old path,
   and refuse as a conflict once the scene's bytes have changed;
5. a real Godot process (outside MCP) loads every scene and resource and its
   semantic tree — node paths and classes, scripts, groups and persistent
   connections — matches what the server's static parser reports.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp.shared.exceptions import McpError  # noqa: E402
from mcp_client import Server, connect, private_godot_environment  # noqa: E402

PLAYER = """extends CharacterBody2D

func _on_coin_body_entered(_body: Node2D) -> void:
\tpass
"""


def vector2(x: float, y: float) -> dict:
    return {"type": "vector2", "value": {"x": x, "y": y}}


def vector3(x: float, y: float, z: float) -> dict:
    return {"x": x, "y": y, "z": z}


VERIFY = """extends SceneTree

func _node_facts(root: Node, node: Node, into: Array) -> void:
\tvar script = node.get_script()
\tvar groups: Array = []
\tfor group in node.get_groups():
\t\tif not String(group).begins_with("_"):
\t\t\tgroups.append(String(group))
\tvar style = node.get("theme_override_styles/normal") if node is Control else null
\tinto.append({
\t\t"path": "." if node == root else String(root.get_path_to(node)),
\t\t"class": node.get_class(),
\t\t"script": script.resource_path if script != null else "",
\t\t"groups": groups,
\t\t"style": style.resource_path if style != null else "",
\t})
\tfor child in node.get_children():
\t\tif child.owner == root:
\t\t\t_node_facts(root, child, into)


func _connections(root: Node, node: Node, into: Array) -> void:
\tfor connection in node.get_incoming_connections():
\t\tvar signal_object: Object = connection["signal"].get_object()
\t\tif (connection["flags"] & CONNECT_PERSIST) != 0 and signal_object is Node:
\t\t\tinto.append({
\t\t\t\t"signal": String(connection["signal"].get_name()),
\t\t\t\t"from": String(root.get_path_to(signal_object)),
\t\t\t\t"to": "." if node == root else String(root.get_path_to(node)),
\t\t\t\t"method": String(connection["callable"].get_method()),
\t\t\t})
\tfor child in node.get_children():
\t\tif child.owner == root:
\t\t\t_connections(root, child, into)


func _init() -> void:
\tvar report := {}
\tfor path in ["res://scenes/main.tscn", "res://scenes/world3d.tscn", "res://scenes/coin.tscn"]:
\t\tvar packed: PackedScene = load(path)
\t\tvar root: Node = packed.instantiate(PackedScene.GEN_EDIT_STATE_MAIN)
\t\tvar nodes: Array = []
\t\t_node_facts(root, root, nodes)
\t\tvar connections: Array = []
\t\t_connections(root, root, connections)
\t\treport[path] = {"nodes": nodes, "connections": connections}
\t\troot.free()
\tvar style: StyleBoxFlat = load("res://ui/panel.tres")
\treport["res://ui/panel.tres"] = {
\t\t"class": style.get_class(),
\t\t"bg_color": [style.bg_color.r, style.bg_color.g, style.bg_color.b, style.bg_color.a],
\t\t"corner_radius": style.corner_radius_top_left,
\t}
\tprint("VERIFY ", JSON.stringify(report))
\tquit(0)
"""


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


async def author(arguments: argparse.Namespace, project: Path, report: dict) -> dict:
    trees: dict = {}
    async with connect(Server(binary=arguments.binary, project=project, profile="full",
                              godot=arguments.godot)) as client:
        (await client.call("script.create", {
            "path": "scripts/player.gd", "content": PLAYER, "idempotencyId": "m3-player",
        })).require_ok()

        coin = (await client.call("scene.create", {
            "path": "scenes/coin.tscn", "rootType": "Area2D", "rootName": "Coin",
            "operations": [
                {"op": "add_node", "parent": ".", "name": "Shape", "type": "CollisionShape2D"},
                {"op": "set_subresource", "node": "Shape", "property": "shape", "class": "CircleShape2D",
                 "properties": {"radius": {"type": "float", "value": 8.0}}},
                {"op": "add_group", "node": ".", "group": "coins"},
            ],
            "idempotencyId": "m3-coin",
        })).require_ok()
        assert coin["conclusion"] == "proven_post_state", coin
        assert coin["uid"] and coin["tree"]["uid"] == coin["uid"], coin

        panel = (await client.call("resource.create", {
            "path": "ui/panel.tres", "class": "StyleBoxFlat",
            "operations": [{"op": "set_property", "property": "bg_color", "value": {
                "type": "color", "value": {"red": 0.1, "green": 0.2, "blue": 0.3, "alpha": 1.0}}}],
            "idempotencyId": "m3-panel",
        })).require_ok()

        main = (await client.call("scene.create", {
            "path": "scenes/main.tscn", "rootType": "Node2D", "rootName": "Main",
            "operations": [
                {"op": "add_node", "parent": ".", "name": "Player", "type": "CharacterBody2D"},
                {"op": "attach_script", "node": "Player", "script": "res://scripts/player.gd"},
                {"op": "set_property", "node": "Player", "property": "position", "value": vector2(64, 32)},
                {"op": "add_node", "parent": "Player", "name": "Body", "type": "CollisionShape2D"},
                {"op": "set_subresource", "node": "Player/Body", "property": "shape",
                 "class": "RectangleShape2D", "properties": {"size": vector2(16, 32)}},
                {"op": "instance_scene", "parent": ".", "name": "Coin", "scene": "res://scenes/coin.tscn"},
                {"op": "set_property", "node": "Coin", "property": "position", "value": vector2(128, 32)},
                {"op": "connect_signal", "from": "Coin", "signal": "body_entered", "to": "Player",
                 "method": "_on_coin_body_entered"},
                {"op": "add_group", "node": "Player", "group": "players"},
                {"op": "add_node", "parent": ".", "name": "Hud", "type": "CanvasLayer"},
                {"op": "add_node", "parent": "Hud", "name": "Score", "type": "Label"},
                {"op": "set_property", "node": "Hud/Score", "property": "text",
                 "value": {"type": "string", "value": "Score: 0"}},
                {"op": "set_property", "node": "Hud/Score", "property": "theme_override_styles/normal",
                 "value": {"type": "resource", "value": {"resource": "res://ui/panel.tres",
                                                          "resource_type": "StyleBoxFlat"}}},
            ],
            "idempotencyId": "m3-main",
        })).require_ok()
        assert main["conclusion"] == "proven_post_state", main
        main_text = (project / "scenes" / "main.tscn").read_text(encoding="utf-8")
        assert any(line.startswith("[ext_resource") and 'path="res://ui/panel.tres"' in line
                   for line in main_text.splitlines()), main_text

        world = (await client.call("scene.create", {
            "path": "scenes/world3d.tscn", "rootType": "Node3D", "rootName": "World",
            "operations": [
                {"op": "add_node", "parent": ".", "name": "Camera", "type": "Camera3D"},
                {"op": "set_property", "node": "Camera", "property": "transform", "value": {
                    "type": "transform3_d", "value": {
                        "basis": {"x": vector3(1, 0, 0), "y": vector3(0, 1, 0), "z": vector3(0, 0, 1)},
                        "origin": vector3(0, 2, 6)}}},
                {"op": "add_node", "parent": ".", "name": "Box", "type": "MeshInstance3D"},
                {"op": "set_subresource", "node": "Box", "property": "mesh", "class": "BoxMesh",
                 "properties": {"size": {"type": "vector3", "value": vector3(1, 1, 1)}}},
                {"op": "add_node", "parent": ".", "name": "Sun", "type": "DirectionalLight3D"},
                {"op": "add_group", "node": "Box", "group": "props"},
            ],
            "idempotencyId": "m3-world",
        })).require_ok()
        assert world["conclusion"] == "proven_post_state", world

        edited = (await client.call("resource.batch.apply", {
            "path": "ui/panel.tres", "expectedSha256": panel["after_sha256"],
            "operations": [{"op": "set_property", "property": "corner_radius_top_left",
                            "value": {"type": "integer", "value": 6}}],
            "idempotencyId": "m3-panel-edit",
        })).require_ok()
        assert edited["uid"] == panel["uid"] and edited["tree"]["uid"] == panel["uid"], (panel, edited)

        # --- one atomic batch over the 2D scene --------------------------------
        batch = (await client.call("scene.batch.apply", {
            "path": "scenes/main.tscn", "expectedSha256": main["after_sha256"],
            "operations": [
                {"op": "rename_node", "node": "Player", "name": "Hero"},
                {"op": "move_node", "node": "Hud/Score", "parent": "Hero"},
                {"op": "set_property", "node": "Hero/Score", "property": "text",
                 "value": {"type": "string", "value": "Hero"}},
                {"op": "add_group", "node": "Hero", "group": "heroes"},
            ],
            "idempotencyId": "m3-batch",
        })).require_ok()
        assert batch["conclusion"] == "proven_post_state", batch
        assert batch["uid"] == main["uid"], (main["uid"], batch["uid"])
        before_refusal = (project / "scenes" / "main.tscn").read_bytes()
        try:
            refused = await client.call("scene.batch.apply", {
                "path": "scenes/main.tscn", "expectedSha256": batch["after_sha256"],
                "operations": [{"op": "remove_node", "node": "NoSuchNode"}],
                "idempotencyId": "m3-refused",
            })
            raise AssertionError(f"an unappliable batch must be refused: {refused.raw_text}")
        except McpError as error:
            # Invalid input is a protocol-level refusal; it names the operation.
            assert "node_not_found at operation 0 (NoSuchNode)" in str(error), str(error)
        assert (project / "scenes" / "main.tscn").read_bytes() == before_refusal

        # --- stale and externally changed preconditions ------------------------
        main_path = project / "scenes" / "main.tscn"
        before_stale = main_path.read_bytes()
        stale = await client.call("scene.batch.apply", {
            "path": "scenes/main.tscn", "expectedSha256": main["after_sha256"],
            "operations": [{"op": "remove_node", "node": "Coin"}],
            "idempotencyId": "m3-stale",
        })
        assert stale.is_error and stale.payload["code"] == "conflict", stale.raw_text
        assert main_path.read_bytes() == before_stale
        main_path.write_bytes(before_stale + b"; edited outside the server\n")
        external = await client.call("scene.batch.apply", {
            "path": "scenes/main.tscn", "expectedSha256": sha256(before_stale),
            "operations": [{"op": "remove_node", "node": "Coin"}],
            "idempotencyId": "m3-external",
        })
        assert external.is_error and external.payload["code"] == "conflict", external.raw_text
        assert main_path.read_bytes() == before_stale + b"; edited outside the server\n"
        main_path.write_bytes(before_stale)

        # --- revision-bound node handles ----------------------------------------
        read = (await client.call("scene.tree.get", {"scene": "scenes/main.tscn"})).require_ok()["data"]
        assert read["sha256"] == sha256(before_stale), read
        handles = {node["path"]: node["handle"] for node in read["nodes"]}
        # The second operation names the node the first one renamed.
        followed = (await client.call("scene.batch.apply", {
            "path": "scenes/main.tscn", "expectedSha256": read["sha256"],
            "operations": [
                {"op": "rename_node", "node": handles["Hud"], "name": "Overlay"},
                {"op": "add_group", "node": handles["Hud"], "group": "ui"},
            ],
            "idempotencyId": "m3-handles",
        })).require_ok()
        assert followed["conclusion"] == "proven_post_state", followed
        current = {node["path"]: node["handle"] for node in followed["tree"]["nodes"]}
        before_replacement = main_path.read_bytes()
        try:
            replaced = await client.call("scene.batch.apply", {
                "path": "scenes/main.tscn", "expectedSha256": followed["after_sha256"],
                "operations": [
                    {"op": "remove_node", "node": "Coin"},
                    {"op": "add_node", "parent": ".", "name": "Coin", "type": "Node2D"},
                    {"op": "set_property", "node": current["Coin"], "property": "position",
                     "value": vector2(1, 1)},
                ],
                "idempotencyId": "m3-replacement",
            })
            raise AssertionError(f"a handle must not name a replacement: {replaced.raw_text}")
        except McpError as error:
            assert "node_not_found at operation 2" in str(error), str(error)
        assert main_path.read_bytes() == before_replacement
        stale_handle = await client.call("scene.batch.apply", {
            "path": "scenes/main.tscn", "expectedSha256": followed["after_sha256"],
            "operations": [{"op": "remove_node", "node": handles["Coin"]}],
            "idempotencyId": "m3-stale-handle",
        })
        assert stale_handle.is_error and stale_handle.payload["code"] == "conflict", stale_handle.raw_text
        assert "stale" in stale_handle.raw_text, stale_handle.raw_text
        assert main_path.read_bytes() == before_replacement

        for path in ("scenes/main.tscn", "scenes/world3d.tscn", "scenes/coin.tscn"):
            tree = (await client.call("scene.tree.get", {"scene": path})).require_ok()
            trees[path] = tree
        trees["batch"] = followed["tree"]
        trees["world"] = world["tree"]
        trees["coin"] = coin["tree"]
        report["authored"] = {
            "main_sha256": followed["after_sha256"],
            "world_sha256": world["after_sha256"],
            "panel_uid": panel["uid"],
            "main_uid": main["uid"],
        }
        assert str(project) not in "\n".join(client.transcript), "an answer leaked the project path"
    return trees


def engine_view(godot: Path, project: Path, scratch: Path) -> dict:
    script = scratch / "verify_scenes.gd"
    script.write_text(VERIFY, encoding="utf-8")
    home = scratch / "home"
    home.mkdir(exist_ok=True)
    completed = subprocess.run(
        [str(godot), "--headless", "--path", str(project), "--script", str(script)],
        capture_output=True, text=True, timeout=180, env=private_godot_environment(home),
    )
    line = next((row for row in completed.stdout.splitlines() if row.startswith("VERIFY ")), None)
    assert line is not None, (completed.returncode, completed.stdout[-2000:], completed.stderr[-2000:])
    return json.loads(line[len("VERIFY "):])


def compare(static: dict, engine: dict) -> None:
    """The static parser and the engine agree on every node, script, group and connection."""
    nodes = {node["path"]: node for node in static["nodes"]}
    engine_nodes = {node["path"]: node for node in engine["nodes"]}
    assert set(nodes) == set(engine_nodes), (sorted(nodes), sorted(engine_nodes))
    for path, node in nodes.items():
        live = engine_nodes[path]
        if node["type"] is not None:
            assert node["type"] == live["class"], (path, node, live)
        assert (node["script"] or "") == live["script"], (path, node, live)
        if node["instance"]:
            # An instance also carries the groups its own scene declares.
            assert set(node["groups"]) <= set(live["groups"]), (path, node, live)
        else:
            assert sorted(node["groups"]) == sorted(live["groups"]), (path, node, live)
    static_connections = sorted(
        (c["signal"], c["from"], c["to"], c["method"]) for c in static["connections"]
    )
    engine_connections = sorted(
        (c["signal"], c["from"], c["to"], c["method"]) for c in engine["connections"]
    )
    assert static_connections == engine_connections, (static_connections, engine_connections)


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    report: dict = {}
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m3-") as scratch:
        scratch_path = Path(scratch).resolve()
        project = scratch_path / "game"
        project.mkdir()
        (project / "project.godot").write_text(
            'config_version=5\n\n[application]\n\nconfig/name="m3"\n', encoding="utf-8"
        )
        trees = await author(arguments, project, report)
        engine = engine_view(arguments.godot, project, scratch_path)
        compare(trees["batch"], engine["res://scenes/main.tscn"])
        compare(trees["world"], engine["res://scenes/world3d.tscn"])
        compare(trees["coin"], engine["res://scenes/coin.tscn"])
        main_nodes = {node["path"]: node for node in engine["res://scenes/main.tscn"]["nodes"]}
        assert "Hero" in main_nodes and "Hero/Score" in main_nodes and "Player" not in main_nodes, main_nodes
        assert sorted(main_nodes["Hero"]["groups"]) == ["heroes", "players"], main_nodes["Hero"]
        assert "Hud" not in main_nodes and main_nodes["Overlay"]["groups"] == ["ui"], main_nodes
        assert main_nodes["Hero/Score"]["style"] == "res://ui/panel.tres", main_nodes["Hero/Score"]
        assert main_nodes["Hero"]["script"] == "res://scripts/player.gd", main_nodes["Hero"]
        assert engine["res://scenes/main.tscn"]["connections"] == [{
            "signal": "body_entered", "from": "Coin", "to": "Hero", "method": "_on_coin_body_entered",
        }], engine["res://scenes/main.tscn"]["connections"]
        panel = engine["res://ui/panel.tres"]
        assert panel["class"] == "StyleBoxFlat" and panel["corner_radius"] == 6, panel
        assert abs(panel["bg_color"][2] - 0.3) < 1e-4, panel
        report["engine"] = {
            "main_nodes": sorted(main_nodes),
            "world_nodes": sorted(node["path"] for node in engine["res://scenes/world3d.tscn"]["nodes"]),
            "panel": panel,
        }
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
