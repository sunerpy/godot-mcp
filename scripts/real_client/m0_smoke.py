"""M0 real-entry smoke: one canonical project context and classified runs.

Usage:
    python3 -I scripts/real_client/m0_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK
against a real Godot editor build:

1. a server started through a *subdirectory* spelling runs the project with
   the discovered root as `--path` (a clean main scene passes);
2. a second server through a *relative* spelling is refused with the
   ownership exit code before it creates a state directory;
3. a game that reports an error and exits nonzero, and a project whose main
   scene cannot be loaded, both return a completed call with
   `classification == "failed"`, the exit code and the parsed engine error; and
4. no response carries the project's absolute path or the private state root.

Exit status 0 means every assertion held; any failure raises.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect  # noqa: E402

PROJECT_GODOT = """config_version=5

[application]
config/name="m0-smoke"
run/main_scene="res://main.tscn"
"""

GOOD_SCRIPT = """extends Node

func _ready() -> void:
\tprint("m0 smoke ready")
\tget_tree().quit()
"""

# A real engine does not exit nonzero for a script error in a running game: it
# keeps running. A nonzero exit comes from the game itself or from a scene the
# engine cannot load, so those are the two failing fixtures.
FAILING_SCRIPT = """extends Node

func _ready() -> void:
\tpush_error("m0 smoke failure")
\tget_tree().quit(3)
"""

MAIN_SCENE = """[gd_scene load_steps=2 format=3]

[ext_resource type="Script" path="res://main.gd" id="1"]

[node name="Main" type="Node"]
script = ExtResource("1")
"""


def make_project(base: Path, script: str, main_scene: str) -> Path:
    root = base / "game"
    (root / "sub" / "deeper").mkdir(parents=True)
    (root / "project.godot").write_text(
        PROJECT_GODOT.replace("res://main.tscn", main_scene), encoding="utf-8"
    )
    (root / "main.gd").write_text(script, encoding="utf-8")
    (root / "main.tscn").write_text(MAIN_SCENE, encoding="utf-8")
    return root


def state_dirs(base: Path) -> list[str]:
    return sorted(str(path.relative_to(base)) for path in base.rglob(".godot-mcp") if path.is_dir())


async def run_case(
    binary: Path, godot: Path, base: Path, script: str, main_scene: str, expect: dict
) -> dict:
    root = make_project(base, script, main_scene)
    async with connect(Server(binary=binary, project=root / "sub", profile="runtime", godot=godot)) as client:
        before = state_dirs(base)
        refused = subprocess.run(
            [str(binary), "serve", "--stdio", "--project", "game/sub/deeper", "--profile", "core"],
            cwd=base,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        assert refused.returncode == 69, (refused.returncode, refused.stderr)
        assert "already owned" in refused.stderr, refused.stderr
        assert state_dirs(base) == before, (before, state_dirs(base))

        result = await client.call("project.run", {"headless": True, "deadline_ms": 120_000})
        payload = result.require_ok()
        assert payload["classification"] == expect["classification"], json.dumps(payload, indent=2)
        assert payload["exit_code"] == expect["exit_code"], json.dumps(payload, indent=2)
        if expect.get("diagnostic"):
            assert any(
                expect["diagnostic"] in diagnostic["message"] for diagnostic in payload["diagnostics"]
            ), json.dumps(payload["diagnostics"], indent=2)
        if expect.get("stdout"):
            assert expect["stdout"] in payload["stdout"]["text"], payload["stdout"]
        for tool, arguments in [("health.get", {}), ("project.info.get", {})]:
            (await client.call(tool, arguments)).require_ok()
        transcript = "\n".join(client.transcript)
        for private in (str(root), str(root / ".godot-mcp")):
            assert private not in transcript, f"{private} leaked"
        return {
            "classification": payload["classification"],
            "exit_code": payload["exit_code"],
            "diagnostics": [d["category"] for d in payload["diagnostics"]],
            "execution": payload["execution"],
            "state_dirs": state_dirs(base),
        }


def engine_version(godot: Path) -> tuple[int, int]:
    """The engine's (major, minor), from its own `--version` line."""
    line = subprocess.run([str(godot), "--version"], capture_output=True, text=True, timeout=120).stdout
    major, minor = line.strip().split(".")[:2]
    return int(major), int(minor)


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    arguments = parser.parse_args()
    report = {}
    cases = [
        ("clean", GOOD_SCRIPT, "res://main.tscn",
         {"classification": "passed", "exit_code": 0, "stdout": "m0 smoke ready"}),
        ("nonzero", FAILING_SCRIPT, "res://main.tscn",
         {"classification": "failed", "exit_code": 3, "diagnostic": "m0 smoke failure"}),
        # Godot's macOS builds from 4.5 on report the unloadable main scene and
        # still exit 0 (4.4.1 exits 1 there, as every build does elsewhere); the
        # verdict is `failed` from the engine's own diagnostics either way.
        ("missing-scene", GOOD_SCRIPT, "res://missing.tscn",
         {"classification": "failed",
          "exit_code": 0 if sys.platform == "darwin" and engine_version(arguments.godot) >= (4, 5) else 1,
          "diagnostic": "Failed loading scene"}),
    ]
    for label, script, main_scene, expect in cases:
        with tempfile.TemporaryDirectory(prefix=f"godot-mcp-m0-{label}-") as scratch:
            report[label] = await run_case(
                arguments.binary.resolve(),
                arguments.godot.resolve(),
                Path(scratch).resolve(),
                script,
                main_scene,
                expect,
            )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
