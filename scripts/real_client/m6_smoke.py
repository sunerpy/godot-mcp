"""M6 real-entry smoke: managed exports through a real Godot build's exact templates.

Usage:
    python3 -I -B scripts/real_client/m6_smoke.py --binary <godot-mcp> --godot <Godot editor>

Drives the built `godot-mcp` binary through the official MCP Python SDK against a real
Godot editor build. Export templates are looked up where Godot looks for them, from this
process's environment (`XDG_DATA_HOME`, else `~/.local/share`, on Linux; `HOME` on macOS;
`APPDATA` on Windows).

1. `export.presets.list` names each preset's template and whether exactly this build's
   template is installed, and names (never shows) a secret written into a preset.
2. With this build's templates installed: a Linux preset exports into a private artifact;
   its bytes, read back in ranges through `resources/read`, hash to the reported SHA-256,
   and a range past its end is refused; nothing is written where the preset's export path
   points. A Windows preset exports too, and, in a project of its own, a macOS preset
   exports its app bundle as one zip that holds the bundle's Info.plist and executable.
   Every export asks for a smoke run: the build of this host's own platform must run
   headless once and exit cleanly — the Linux build on Linux, the Windows build on
   Windows, the macOS bundle's executable on a Mac — and the other two must report that
   this host cannot run them. After a server restart the same local stdio principal still
   reads and lists the artifact (it is retained until it expires). A fresh project's
   export is cancelled while it imports and leaves no artifact.
3. Without this build's templates: the export is refused before anything runs.
4. Either way: a preset with an inline secret, and an unsupported platform, are refused.
5. The same server on authenticated `trusted_local` HTTP (the binary must be built with
   the `http` feature): with templates, an export over HTTP reads back in ranges to its
   reported SHA-256 and refuses a range past its end, exactly as over stdio; a second
   HTTP session neither reads nor lists it; the artifact the stdio principal produced is
   not readable over HTTP. Without templates, the export is refused the same way.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import io
import json
import os
import random
import struct
import sys
import tempfile
import time
import zipfile
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcp_client import Server, connect, connect_http  # noqa: E402

PROJECT = """config_version=5

[application]

config/name="m6"
run/main_scene="res://main.tscn"

[rendering]

renderer/rendering_method="gl_compatibility"
textures/vram_compression/import_etc2_astc=true
"""

MAIN = """[gd_scene load_steps=2 format=3]

[ext_resource type="Script" path="res://main.gd" id="1"]

[node name="Main" type="Node2D"]
script = ExtResource("1")
"""

MAIN_SCRIPT = """extends Node2D

func _ready() -> void:
\tprint("m6 exported game ready")
"""

SECRET = "9b1d0c4e7f2a6b8d3e5c1a7f9b2d4c6e8a0b2d4f6a8c0e2b4d6f8a0c2e4b6d8f"

PRESETS = f"""[preset.0]

name="Linux"
platform="Linux"
runnable=true
export_filter="all_resources"
export_path="build/m6.x86_64"

[preset.0.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=true
binary_format/architecture="x86_64"

[preset.1]

name="Windows"
platform="Windows Desktop"
runnable=true
export_filter="all_resources"
export_path="build/m6.exe"

[preset.1.options]

custom_template/debug=""
custom_template/release=""
binary_format/embed_pck=true
binary_format/architecture="x86_64"
debug/export_console_wrapper=0
codesign/enable=false

[preset.2]

name="Secret"
platform="Linux"
runnable=false
export_path="build/secret.x86_64"
script_encryption_key="{SECRET}"

[preset.2.options]

binary_format/architecture="x86_64"

[preset.3]

name="Phone"
platform="Android"
runnable=false
export_path="build/m6.apk"

[preset.3.options]

[preset.4]

name="Mac"
platform="macOS"
runnable=true
export_filter="all_resources"
export_path="build/m6.app"

[preset.4.options]

custom_template/debug=""
custom_template/release=""
binary_format/architecture="universal"
application/bundle_identifier="org.godotmcp.m6"
codesign/codesign=1
"""


def noise_png(path: Path, size: int, seed: int) -> None:
    """A PNG of random pixels: slow to import, so an export is still importing when cancelled."""
    generator = random.Random(seed)
    rows = b"".join(b"\x00" + generator.randbytes(size * 3) for _ in range(size))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
                     + chunk(b"IDAT", zlib.compress(rows, 1)) + chunk(b"IEND", b""))


def write_project(project: Path, heavy: bool) -> None:
    project.mkdir()
    (project / "project.godot").write_text(PROJECT, encoding="utf-8")
    (project / "main.tscn").write_text(MAIN, encoding="utf-8")
    (project / "main.gd").write_text(MAIN_SCRIPT, encoding="utf-8")
    (project / "export_presets.cfg").write_text(PRESETS, encoding="utf-8")
    if heavy:
        art = project / "art"
        art.mkdir()
        for index in range(6):
            noise_png(art / f"noise{index}.png", 2048, index)


def templates_root() -> Path:
    """Where this host's Godot keeps export templates, the way the server looks."""
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "Godot" / "export_templates"
    if sys.platform == "win32":
        return Path(os.environ["APPDATA"]) / "Godot" / "export_templates"
    data = os.environ.get("XDG_DATA_HOME")
    base = Path(data) if data and os.path.isabs(data) else Path.home() / ".local" / "share"
    return base / "godot" / "export_templates"


# The preset whose export this host runs once: every host runs its own platform's build.
HOST_PRESET = {"linux": "Linux", "win32": "Windows", "darwin": "Mac"}[sys.platform]


def check_smoke(preset: str, smoke: dict, executable: str) -> None:
    """This host's own build must have run and exited cleanly, from `executable`; any other
    build must report that this host cannot run it."""
    if preset == HOST_PRESET:
        assert smoke["ran"] is True and smoke["passed"] is True, smoke
        assert smoke["termination"] == "exited_clean", smoke
        assert smoke["executable"] == executable, smoke
    else:
        assert smoke["ran"] is False and "cannot run" in (smoke.get("reason") or ""), smoke


async def ended(client, export: str, timeout: float = 300.0) -> dict:
    deadline = time.monotonic() + timeout
    while True:
        status = (await client.call("project.export.status", {"export": export})).require_ok()
        if status["state"] not in ("running", "starting"):
            return status
        if time.monotonic() > deadline:
            raise AssertionError(f"the export never ended: {status}")
        await asyncio.sleep(0.2)


CHUNK = 4 * 1024 * 1024


async def read_range(client, uri: str, start: int, end: int) -> bytes:
    """One inclusive byte range of an artifact, through `resources/read`."""
    from mcp import types

    request = types.ClientRequest(types.ReadResourceRequest(
        params=types.ReadResourceRequestParams(
            uri=uri,
            _meta={"io.godot-mcp/artifactRange": {"start": start, "endInclusive": end}},
        ),
    ))
    result = await client.session.send_request(request, types.ReadResourceResult)
    return base64.b64decode(result.contents[0].blob)


async def read_artifact(client, uri: str, total: int, chunk: int = CHUNK) -> bytes:
    """Read an artifact in inclusive byte ranges through `resources/read`."""
    data = bytearray()
    while len(data) < total:
        data.extend(await read_range(client, uri, len(data), min(total, len(data) + chunk) - 1))
    return bytes(data)


async def read_refused(client, uri: str, start: int | None = None, end: int | None = None) -> str:
    """The refusal of a read that must fail: of a whole artifact, or of one range."""
    from mcp.shared.exceptions import McpError

    try:
        if start is None:
            await client.read_resource(uri)
        else:
            await read_range(client, uri, start, end)
    except McpError as error:
        return str(error)
    raise AssertionError(f"reading {uri} was not refused")


async def listed_artifacts(client) -> list[str]:
    listed = await client.session.list_resources()
    return sorted(str(resource.uri) for resource in listed.resources
                  if str(resource.uri).startswith("godot-mcp://artifacts/"))


async def refused(client, name: str, arguments: dict) -> str:
    """The refusal text of a call that must fail: a tool error, or the protocol error an
    `invalid_input` refusal arrives as."""
    from mcp.shared.exceptions import McpError

    try:
        result = await client.call(name, arguments)
    except McpError as error:
        return str(error)
    assert result.is_error, result.raw_text
    return result.raw_text


async def refusals(client, report: dict) -> None:
    secret = await refused(client, "project.export", {"preset": "Secret", "idempotencyId": "m6-secret"})
    assert "script_encryption_key" in secret, secret
    phone = await refused(client, "project.export", {"preset": "Phone", "idempotencyId": "m6-phone"})
    assert "unsupported" in phone or "not supported" in phone, phone
    assert all(SECRET not in entry for entry in client.transcript), "a secret value left the server"
    report["refused"] = {"secret_preset": True, "unsupported_platform": True}


async def http_phase(server: Server, stdio_artifact: str | None, label: str, report: dict) -> None:
    """The same server on authenticated HTTP: the same answers, bound to the session."""
    async with connect_http(server) as http:
        async with http.session() as owner:
            if stdio_artifact is None:
                missing = await refused(owner, "project.export",
                                        {"preset": "Linux", "idempotencyId": "m6-http-missing"})
                assert label in missing, missing
                report["http"] = {"missing_template_refused": True}
                return
            started = (await owner.call("project.export", {
                "preset": "Linux", "idempotencyId": "m6-http-linux"})).require_ok()
            status = await ended(owner, started["export"]["session"])
            assert status["export"]["outcome"] == "succeeded", status
            output = status["export"]["outputs"][0]
            data = await read_artifact(owner, output["artifact"], output["bytes"])
            digest = hashlib.sha256(data).hexdigest()
            assert digest == output["sha256"], (digest, output["sha256"])
            past_end = await read_refused(owner, output["artifact"], output["bytes"] - 4, output["bytes"])
            listed = await listed_artifacts(owner)
            assert output["artifact"] in listed, listed
            assert stdio_artifact not in listed, "another principal's artifact is listed"
            foreign = await read_refused(owner, stdio_artifact, 0, 15)
            async with http.session() as sibling:
                sibling_read = await read_refused(sibling, output["artifact"], 0, 15)
                assert output["artifact"] not in await listed_artifacts(sibling), "a sibling lists it"
            again = await read_range(owner, output["artifact"], 0, 15)
            assert again == data[:16], "the owner lost its artifact after a refusal elsewhere"
            report["http"] = {
                "bytes": output["bytes"], "sha256": output["sha256"], "artifact_verified": True,
                "past_end_refused": bool(past_end), "sibling_session_refused": bool(sibling_read),
                "stdio_principal_artifact_refused": bool(foreign),
            }


async def flow(arguments: argparse.Namespace, scratch: Path, report: dict) -> None:
    project = scratch / "game"
    write_project(project, heavy=False)
    server = Server(binary=arguments.binary, project=project, profile="full", godot=arguments.godot)
    async with connect(server) as client:
        listed = (await client.call("export.presets.list")).require_ok()
        presets = {preset["name"]: preset for preset in listed["presets"]}
        linux = presets["Linux"]["template"]
        assert linux["file"] == "linux_release.x86_64", linux
        label = linux["label"]
        assert label and label.endswith(".stable"), linux
        assert presets["Secret"]["secret_options"] == ["script_encryption_key"], presets["Secret"]
        assert "unsupported" in presets["Phone"]["template"], presets["Phone"]
        installed = (templates_root() / label / "version.txt").is_file()
        assert linux["installed"] is installed, (linux, installed)
        report["template"] = {"label": label, "installed": installed}
        await refusals(client, report)

        if not installed:
            missing = await refused(client, "project.export", {"preset": "Linux", "idempotencyId": "m6-missing"})
            assert label in missing, missing
            report["missing_template_refused"] = True
    # Where the export itself is the gate, a run without templates proves only
    # the refusal above and is a failure.
    assert installed or not arguments.require_templates, (
        f"the {label} export templates are not installed under {templates_root()}")
    if not installed:
        await http_phase(server, None, label, report)
        return
    async with connect(server) as client:
        started = (await client.call("project.export", {
            "preset": "Linux", "smoke": True, "idempotencyId": "m6-linux"})).require_ok()
        export = started["export"]["session"]
        status = await ended(client, export)
        assert status["export"]["outcome"] == "succeeded", status
        output = status["export"]["outputs"][0]
        assert output["name"] == "m6.x86_64", status
        data = await read_artifact(client, output["artifact"], output["bytes"])
        assert len(data) == output["bytes"], (len(data), output["bytes"])
        digest = hashlib.sha256(data).hexdigest()
        assert digest == output["sha256"], (digest, output["sha256"])
        past_end = await read_refused(client, output["artifact"], output["bytes"] - 4, output["bytes"])
        assert "exceeds" in past_end, past_end
        prefix = data[:CHUNK]
        smoke = status["export"]["smoke"]
        check_smoke("Linux", smoke, "m6.x86_64")
        assert not (project / "build").exists(), "the export wrote into the project"
        report["linux"] = {"bytes": output["bytes"], "sha256": output["sha256"], "smoke": smoke,
                           "artifact_verified": True}

        windows = (await client.call("project.export", {
            "preset": "Windows", "smoke": True, "idempotencyId": "m6-windows"})).require_ok()
        status = await ended(client, windows["export"]["session"])
        assert status["export"]["outcome"] == "succeeded", status
        names = sorted(output["name"] for output in status["export"]["outputs"])
        assert "m6.exe" in names, names
        windows_smoke = status["export"]["smoke"]
        check_smoke("Windows", windows_smoke, "m6.exe")
        report["windows"] = {"outputs": names, "smoke": windows_smoke}
        artifact_uri = output["artifact"]

    # Over stdio the artifact belongs to the local principal and this project,
    # not to one process: a restarted server still serves it until it expires.
    async with connect(server) as client:
        again = await read_range(client, artifact_uri, 0, len(prefix) - 1)
        assert again == prefix, "the restarted server served different bytes"
        assert artifact_uri in await listed_artifacts(client), "the owner no longer lists it"
        report["artifact_survives_restart_for_owner"] = True

    await http_phase(server, artifact_uri, label, report)

    # A macOS app bundle, in a project of its own, so its artifact has that project's
    # artifact store to itself.
    mac = scratch / "mac"
    write_project(mac, heavy=False)
    async with connect(Server(binary=arguments.binary, project=mac, profile="full",
                              godot=arguments.godot)) as client:
        listed = (await client.call("export.presets.list")).require_ok()
        template = next(preset for preset in listed["presets"] if preset["name"] == "Mac")["template"]
        assert template["file"] == "macos.zip" and template["installed"] is True, template
        assert template["runnable_here"] is (HOST_PRESET == "Mac"), template
        started = (await client.call("project.export", {
            "preset": "Mac", "smoke": True, "idempotencyId": "m6-mac"})).require_ok()
        status = await ended(client, started["export"]["session"])
        assert status["export"]["outcome"] == "succeeded", status
        outputs = status["export"]["outputs"]
        assert [output["name"] for output in outputs] == ["m6.zip"], outputs
        data = await read_artifact(client, outputs[0]["artifact"], outputs[0]["bytes"])
        digest = hashlib.sha256(data).hexdigest()
        assert digest == outputs[0]["sha256"], (digest, outputs[0]["sha256"])
        with zipfile.ZipFile(io.BytesIO(data)) as bundle:
            entries = bundle.namelist()
        for expected in ("m6.app/Contents/Info.plist", "m6.app/Contents/MacOS/m6",
                         "m6.app/Contents/Resources/m6.pck"):
            assert expected in entries, (expected, entries[:40])
        mac_smoke = status["export"]["smoke"]
        check_smoke("Mac", mac_smoke, "m6.app/Contents/MacOS/m6")
        assert not (mac / "build").exists(), "the export wrote into the project"
        report["mac"] = {"bytes": outputs[0]["bytes"], "sha256": outputs[0]["sha256"],
                         "entries": len(entries), "smoke": mac_smoke, "artifact_verified": True}
    smokes = {"Linux": report["linux"]["smoke"], "Windows": report["windows"]["smoke"],
              "Mac": report["mac"]["smoke"]}
    report["host_smoke"] = {"preset": HOST_PRESET, **smokes[HOST_PRESET]}

    # Cancel a fresh project's export while it is still importing.
    heavy = scratch / "heavy"
    write_project(heavy, heavy=True)
    async with connect(Server(binary=arguments.binary, project=heavy, profile="full",
                              godot=arguments.godot)) as client:
        started = (await client.call("project.export", {
            "preset": "Linux", "idempotencyId": "m6-cancel"})).require_ok()
        export = started["export"]["session"]
        cancelled = (await client.call("project.export.cancel", {
            "export": export, "idempotencyId": "m6-cancel-1"})).require_ok()
        assert cancelled["state"] == "cancelled", cancelled
        status = (await client.call("project.export.status", {"export": export})).require_ok()
        assert status["export"]["outcome"] == "cancelled" and status["export"]["outputs"] == [], status
        assert status["exit"]["boundary_empty"] is True, status
        report["cancel"] = {"state": "cancelled", "outputs": 0}


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--godot", type=Path, required=True)
    parser.add_argument("--require-templates", action="store_true",
                        help="fail instead of checking only the refusal when templates are missing")
    arguments = parser.parse_args()
    arguments.binary = arguments.binary.resolve()
    arguments.godot = arguments.godot.resolve()
    report: dict = {"godot": arguments.godot.name}
    with tempfile.TemporaryDirectory(prefix="godot-mcp-m6-") as scratch:
        await flow(arguments, Path(scratch).resolve(), report)
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
