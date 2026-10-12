"""Measure the facts `docs/operator/usage-and-verification.md` reports.

Usage:
    python3 -I -B scripts/real_client/report_measure.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK and prints one
JSON report:

* the handshake: the protocol version, `serverInfo` and the capability groups declared;
* forty `project.info.get` calls sent at once on one session, and how many the
  per-subject concurrency limiter refused;
* per profile: how many tools `tools/list` names, how many concrete resources
  `resources/list` returns and how many templates `resources/templates/list` returns;
* under `full`, with the real Godot build configured: every tool called once, serially,
  with nothing but an `idempotencyId`, and the answer sorted into a tier — `answered`
  (neither a protocol error nor `isError`), `missing_argument` (protocol error -32602),
  `refused` (any other refusal) or `timeout` — with the refusal's first line;
* the analysis calls with real arguments the report tabulates, on its fixture project;
* `health.get` with no Godot executable resolvable at all;
* a server whose `godot-mcp-supervisor` is not beside it: its `godot` health component
  and its `project.import.run` refusal, both scanned for host paths.

Each server is a fresh `serve --stdio` process on a fresh copy of the fixture project.
Managed sessions a call starts are stopped before the server exits.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect  # noqa: E402

PROJECT = """config_version=5

[application]

config/name="report fixture"
run/main_scene="res://scenes/main.tscn"
"""

PLAYER = """class_name CorePlayer
extends Node

signal scored(points: int)

var score := 0


func add_points(points: int) -> void:
\tscore += points
\tscored.emit(points)
"""

MAIN = """[gd_scene load_steps=3 format=3]

[ext_resource type="Script" path="res://scripts/player.gd" id="1"]
[ext_resource type="PackedScene" path="res://scenes/hud.tscn" id="2"]

[node name="Main" type="Node"]

[node name="Player" type="Node" parent="."]
script = ExtResource("1")

[node name="Hud" parent="." instance=ExtResource("2")]

[node name="Label" type="Label" parent="."]
text = "ready"

[connection signal="scored" from="Player" to="." method="_on_scored"]
"""

HUD = """[gd_scene format=3]

[node name="Hud" type="CanvasLayer"]
"""

BUFF = """[gd_resource type="Resource" format=3]

[resource]
"""

SCHEMA_REAL_ARGUMENTS = [
    ("script.validate", {"script": "scripts/player.gd"}),
    ("gdscript.symbols.get", {"script": "scripts/player.gd"}),
    ("gdscript.references.get", {"script": "scripts/player.gd"}),
    ("gdscript.impact.get", {"script": "scripts/player.gd"}),
    ("scene.tree.get", {"scene": "scenes/main.tscn"}),
    ("scene.dependencies.get", {"scene": "scenes/main.tscn"}),
    ("resource.tree.get", {"resource": "resources/buff.tres"}),
    ("resource.tree.get", {"resource": "resources/legacy.res"}),
    ("scene.tree.get", {"scene": "scenes/legacy.scn"}),
]


def fixture(root: Path) -> Path:
    project = root / "game"
    for relative, text in [
        ("project.godot", PROJECT),
        ("scripts/player.gd", PLAYER),
        ("scenes/main.tscn", MAIN),
        ("scenes/hud.tscn", HUD),
        ("resources/buff.tres", BUFF),
    ]:
        path = project / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    # Binary formats the analyser declines rather than guesses at.
    (project / "scenes" / "legacy.scn").write_bytes(b"RSCC\x00\x00\x00\x00binary scene")
    (project / "resources" / "legacy.res").write_bytes(b"RSRC\x00\x00\x00\x00binary resource")
    return project


def first_line(text: str) -> str:
    return text.strip().splitlines()[0][:300] if text.strip() else ""


def summary(payload) -> dict:
    """The verdict-like facts of an answered call, when it carries any."""
    data = payload.get("data", payload) if isinstance(payload, dict) else {}
    if not isinstance(data, dict):
        return {}
    facts = {key: data[key] for key in ("verdict", "status", "state", "framework", "installed")
             if key in data and not isinstance(data[key], (dict, list))}
    if isinstance(data.get("reason"), str):
        facts["reason"] = first_line(data["reason"])
    proof = data.get("proof")
    if isinstance(proof, dict):
        facts["proof"] = {key: proof.get(key) for key in
                          ("kind", "exit_code", "release_status", "boundary_membership",
                           "boundary_empty_after_exit") if key in proof}
    return facts


HOST_PATHS = ("/config/", "/tmp/", "/home/", "/Users/", "/var/", "/opt/", "/usr/")


async def call(client, name: str, arguments: dict) -> dict:
    from mcp.shared.exceptions import McpError

    try:
        result = await client.call(name, arguments)
    except McpError as error:
        code = getattr(error.error, "code", None)
        tier = "missing_argument" if code == -32602 else "refused"
        return {"tier": tier, "code": code, "message": first_line(str(error))}
    if not result.is_error:
        return {"tier": "answered", **summary(result.payload)}
    payload = result.payload if isinstance(result.payload, dict) else {}
    tier = "timeout" if payload.get("code") == "timeout" else "refused"
    return {"tier": tier, "code": payload.get("code"), "message": first_line(str(payload.get("message", "")))}


async def handshake_and_burst(binary: Path, godot: Path, scratch: Path) -> dict:
    project = fixture(scratch / "handshake")
    async with connect(Server(binary=binary, project=project, profile="full", godot=godot)) as client:
        initialized = client.initialized
        capabilities = initialized.capabilities.model_dump(exclude_none=True)
        answers = await asyncio.gather(*(call(client, "project.info.get", {}) for _ in range(40)))
    refused = [answer for answer in answers if answer["tier"] != "answered"]
    return {
        "protocol_version": initialized.protocolVersion,
        "server_info": initialized.serverInfo.model_dump(exclude_none=True),
        "capabilities": sorted(capabilities),
        "burst": {
            "sent": len(answers),
            "answered": len(answers) - len(refused),
            "refused": len(refused),
            "messages": sorted({answer.get("message", "") for answer in refused}),
        },
    }


async def profiles(binary: Path, godot: Path, scratch: Path) -> dict:
    counts = {}
    for profile in ("core", "editor", "runtime", "full"):
        project = fixture(scratch / f"profile-{profile}")
        async with connect(Server(binary=binary, project=project, profile=profile, godot=godot)) as client:
            tools = await client.tool_names()
            resources = await client.session.list_resources()
            templates = await client.session.list_resource_templates()
            counts[profile] = {
                "tools": len(tools),
                "resources": len(resources.resources),
                "templates": len(templates.resourceTemplates),
            }
    return counts


async def tiers(binary: Path, godot: Path, scratch: Path) -> dict:
    project = fixture(scratch / "tiers")
    answers = {}
    async with connect(Server(binary=binary, project=project, profile="full", godot=godot,
                              extra_config={"limits.deadline_ms": 30000})) as client:
        for index, name in enumerate(await client.tool_names()):
            answers[name] = await call(client, name, {"idempotencyId": f"tier-{index}"})
        # Leave nothing running: stop every session a call started.
        status = (await client.call("project.run.status", {})).payload or {}
        for session in status.get("sessions", []):
            if session.get("state") in ("starting", "running"):
                tool = {"game": "project.run.stop", "editor": "editor.session.stop",
                        "export": "project.export.cancel"}[session["kind"]]
                key = {"game": "session", "editor": "session", "export": "export"}[session["kind"]]
                await client.call(tool, {key: session["session"], "idempotencyId": f"stop-{session['session']}"})
    grouped: dict = {}
    for name, answer in sorted(answers.items()):
        grouped.setdefault(answer["tier"], []).append(name)
    return {"by_tool": answers, "by_tier": {tier: names for tier, names in sorted(grouped.items())},
            "counts": {tier: len(names) for tier, names in sorted(grouped.items())}}


async def real_arguments(binary: Path, godot: Path, scratch: Path) -> list:
    project = fixture(scratch / "real")
    results = []
    async with connect(Server(binary=binary, project=project, profile="full", godot=godot)) as client:
        for name, arguments in SCHEMA_REAL_ARGUMENTS:
            from mcp.shared.exceptions import McpError

            try:
                result = await client.call(name, arguments)
                payload = result.payload
                results.append({"tool": name, "arguments": arguments, "is_error": result.is_error,
                                "payload": payload if isinstance(payload, dict) else str(payload)[:500]})
            except McpError as error:
                results.append({"tool": name, "arguments": arguments, "protocol_error": first_line(str(error))})
    return results


async def health_without_godot(binary: Path, scratch: Path) -> dict:
    project = fixture(scratch / "no-godot")
    saved = os.environ.get("PATH", "")
    os.environ["PATH"] = "/usr/bin:/bin"
    try:
        async with connect(Server(binary=binary, project=project, profile="full")) as client:
            health = (await client.call("health.get", {})).require_ok()
    finally:
        os.environ["PATH"] = saved
    text = json.dumps(health)
    leaked = [needle for needle in ("/config/", "/tmp/", "/home/", ".gd\"", "secret", "token", "bearer", "password")
              if needle in text]
    return {
        "top_level_keys": len(health),
        "codes": health.get("codes"),
        "readiness": health.get("readiness"),
        "engine": health.get("engine"),
        "versions": health.get("versions"),
        "queue": health.get("queue"),
        "journal": health.get("journal"),
        "lease_ownership": (health.get("lease") or {}).get("ownership"),
        "components": health.get("components"),
        "scan_hits": leaked,
    }


async def supervisor_absent(binary: Path, godot: Path, scratch: Path) -> dict:
    alone = scratch / "alone"
    alone.mkdir()
    shutil.copy2(binary, alone / binary.name)
    project = fixture(scratch / "no-supervisor")
    async with connect(Server(binary=alone / binary.name, project=project, profile="full",
                              godot=godot)) as client:
        health = (await client.call("health.get", {})).require_ok()
        component = next(item for item in health["components"] if item["component"] == "godot")
        refusal = await call(client, "project.import.run", {"idempotencyId": "no-supervisor"})
    text = json.dumps([component, refusal, health.get("codes")])
    return {
        "godot_component": component,
        "codes": health.get("codes"),
        "import_refusal": refusal,
        "scan_hits": [needle for needle in HOST_PATHS if needle in text],
    }


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    arguments = parser.parse_args()
    binary, godot = arguments.binary.resolve(), arguments.godot.resolve()
    with tempfile.TemporaryDirectory(prefix="godot-mcp-report-") as scratch:
        root = Path(scratch)
        report = {
            "handshake": await handshake_and_burst(binary, godot, root),
            "profiles": await profiles(binary, godot, root),
            "tiers": await tiers(binary, godot, root),
            "real_arguments": await real_arguments(binary, godot, root),
            "health_without_godot": await health_without_godot(binary, root),
            "supervisor_absent": await supervisor_absent(binary, godot, root),
        }
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
