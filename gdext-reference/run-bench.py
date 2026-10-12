"""Run the ADR 0010 spike against exact Godot builds: load the extension, then
benchmark the probe's tree read in GDScript and natively on the same scene.

Usage:
    python3 -I -B gdext-reference/run-bench.py --library <libgodot_mcp_gdext_spike.so> \
        --godot <Godot editor> [--godot <Godot editor> ...]

Each build gets a fresh scratch project holding the extension's descriptor (written
here, never committed), a copy of the library and `bench/bench.gd`. The project is
imported once, which registers the extension, and the benchmark then runs headless.
One JSON report per build is printed; the exit code is 0 only when every build loaded
the extension and produced identical snapshots from both implementations.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

DESCRIPTOR = """[configuration]

entry_symbol = "gdext_rust_init"
compatibility_minimum = 4.4
reloadable = false

[libraries]

linux.debug.x86_64 = "res://bin/libgodot_mcp_gdext_spike.so"
linux.release.x86_64 = "res://bin/libgodot_mcp_gdext_spike.so"
"""

PROJECT = """config_version=5

[application]

config/name="gdext spike bench"
"""


def run(godot: Path, library: Path, bench: Path) -> dict:
    with tempfile.TemporaryDirectory(prefix="gdext-spike-") as scratch:
        project = Path(scratch)
        (project / "project.godot").write_text(PROJECT, encoding="utf-8")
        (project / "godot_mcp_spike.gdextension").write_text(DESCRIPTOR, encoding="utf-8")
        (project / "bin").mkdir()
        shutil.copy2(library, project / "bin" / library.name)
        shutil.copy2(bench, project / "bench.gd")
        imported = subprocess.run(
            [str(godot), "--headless", "--path", str(project), "--import"],
            capture_output=True, text=True, timeout=300,
        )
        registered = (project / ".godot" / "extension_list.cfg").is_file()
        ran = subprocess.run(
            [str(godot), "--headless", "--path", str(project), "--script", "res://bench.gd"],
            capture_output=True, text=True, timeout=300,
        )
        line = next((line for line in ran.stdout.splitlines() if line.startswith("BENCH ")), None)
        report = json.loads(line[len("BENCH "):]) if line else {"error": "no BENCH line"}
        report.update({
            "binary": godot.name,
            "import_exit": imported.returncode,
            "extension_registered": registered,
            "exit": ran.returncode,
            "stderr_tail": ran.stderr[-600:],
        })
        return report


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--library", type=Path, required=True)
    parser.add_argument("--godot", type=Path, action="append", required=True)
    arguments = parser.parse_args()
    bench = Path(__file__).resolve().parent / "bench" / "bench.gd"
    reports = [run(godot.resolve(), arguments.library.resolve(), bench) for godot in arguments.godot]
    print(json.dumps(reports, indent=2, sort_keys=True))
    return 0 if all(report.get("exit") == 0 and report.get("identical") for report in reports) else 1


if __name__ == "__main__":
    sys.exit(main())
