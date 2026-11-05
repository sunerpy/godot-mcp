"""M1 real-entry smoke: coherent workspace revisions and catalog discovery.

Usage:
    python3 -I -B scripts/real_client/m1_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK:

1. every listed tool carries a title, a specific description, a strict schema
   and MCP annotations, and the `full` profile lists exactly the implemented
   catalog (no handlerless editor/runtime method);
2. an external file created after the first query is answered from the next
   coherent revision;
3. a real Godot canonical re-save (`resource.update`) returns the workspace
   revision that contains its write, and the next read is at least that
   revision.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect  # noqa: E402

PROFILES = Path(__file__).resolve().parents[2] / "schemas" / "domain" / "profiles"


def expected_full() -> set[str]:
    """The tools the newest frozen `full` manifest lists: the catalog a server
    built from this checkout must answer with."""
    versions = sorted(
        (tuple(int(part) for part in path.name[1:].split(".")), path)
        for path in PROFILES.glob("v*.*")
        if path.is_dir()
    )
    manifest = json.loads((versions[-1][1] / "full.json").read_text())
    return set(manifest["tools"])


RESOURCE = """[gd_resource type="Resource" format=3]

[resource]
"""


async def wait_revision(client, after: int, timeout: float = 15.0) -> dict:
    deadline = time.monotonic() + timeout
    while True:
        info = (await client.call("project.info.get")).require_ok()
        if info["workspace"]["revision"] > after:
            return info
        if time.monotonic() > deadline:
            raise AssertionError(f"revision never passed {after}: {info['workspace']}")
        await asyncio.sleep(0.05)


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    arguments = parser.parse_args()
    report: dict = {}
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m1-") as scratch:
        root = Path(scratch).resolve() / "game"
        (root / "scripts").mkdir(parents=True)
        (root / "project.godot").write_text('config_version=5\n\n[application]\nconfig/name="m1"\n')
        (root / "scripts" / "player.gd").write_text("extends Node\n\nfunc jump() -> void:\n\tpass\n")
        (root / "data.tres").write_text(RESOURCE)
        async with connect(Server(binary=arguments.binary.resolve(), project=root,
                                  profile="full", godot=arguments.godot.resolve())) as client:
            tools = await client.tools()
            names = {tool.name for tool in tools}
            expected = expected_full()
            assert names == expected, sorted(names ^ expected)
            for tool in tools:
                assert tool.title, tool.name
                assert len(tool.description or "") >= 40, tool.name
                assert tool.inputSchema.get("additionalProperties") is False, tool.name
                hints = tool.annotations
                assert hints is not None and hints.readOnlyHint is not None, tool.name
            report["listed"] = len(tools)

            first = (await client.call("gdscript.references.get",
                                       {"script": "scripts/player.gd", "symbol": "jump"})).require_ok()
            start = first["workspace"]["revision"]
            assert first["workspace"]["coherent"] is True, first["workspace"]
            (root / "scripts" / "enemy.gd").write_text("extends Node\n\nfunc attack() -> void:\n\tpass\n")
            info = await wait_revision(client, start)
            symbols = (await client.call("gdscript.symbols.get", {"script": "scripts/enemy.gd"})).require_ok()
            assert "attack" in json.dumps(symbols), symbols
            report["external_create_revision"] = info["workspace"]["revision"]

            before = info["workspace"]["revision"]
            update = await client.call("resource.update", {"target": "data.tres", "idempotencyId": "m1-smoke-1"})
            payload = update.require_ok()
            assert payload["conclusion"] == "proven_post_state", payload
            written = payload["workspace"]["revision"]
            assert written > before and payload["workspace"]["coherent"] is True, payload["workspace"]
            after = (await client.call("resource.tree.get", {"resource": "data.tres"})).require_ok()
            assert after["workspace"]["revision"] >= written, after["workspace"]
            report["resource_update"] = {"conclusion": payload["conclusion"], "revision": written}
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
