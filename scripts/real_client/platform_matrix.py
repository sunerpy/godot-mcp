"""Run the real-client checks against every exact Godot build of this host's platform.

Usage:
    python3 -I -B scripts/real_client/platform_matrix.py --binary <godot-mcp> --out <dir>
        [--versions 4.4.1,4.7.1] [--checks m0,m5,e2e] [--cache <dir>] [--data-home <dir>]

For each version it takes this host's locked editor artifact from
`tests/fixtures/godot-releases.lock.json`, reuses a cached copy whose SHA-256 matches or
downloads one and checks it, extracts it once, and runs every check as its own process
with a deadline: the M0-M6 smokes and the complete-game E2E. The E2E commits the GUT
release whose version this script pins for the build's minor version, fetched the same
way. When a check that exports is selected, the build's export templates are installed
from the locked template archive into `--data-home` (default `<work>/data-home`) before
it runs, every check runs with the host's data-directory variable pointing there
(`XDG_DATA_HOME` on Linux, `HOME` on macOS, `APPDATA` on Windows), and M6 runs with
`--require-templates`, so an export check never passes on its refusal path alone.

It writes `<out>/<check>-<version>.txt` and `<out>/summary.json`, and exits 1 when any
check did not pass. Nothing here decides a check's outcome but the check's own exit
status: a check that could not start is recorded as failed, never as skipped.

`godot-mcp-supervisor` must be beside `--binary`. A windowed check needs a display
(on Linux, `DISPLAY`), which this script passes through and never invents.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import time
import urllib.request
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LOCK = REPO / "tests" / "fixtures" / "godot-releases.lock.json"
CHECKS = ("m0", "m1", "m2", "m3", "m4", "m5", "m6", "e2e")
EXPORTING = ("m6", "e2e")
DEADLINE_SECONDS = 1800

# The GUT release each Godot minor version needs, as the archive GitHub serves for its
# tag, with the digest that archive had when it was pinned.
GUT = {
    "4.4": ("9.4.0", "7e057ef8c8546d363b51965f2bdf29a57813682689e430acf4cf227119144a86"),
    "4.5": ("9.5.0", "7f6197e34096b5ca62c2b84b3d268c3f270317dcbcc19d4acfe788a1d2b5e601"),
    "4.6": ("9.6.1", "3901c72603b7573d7772cad2462f50a0f50ba8748e38f8727599344ce3aadfef"),
    "4.7": ("9.7.1", "14969aa46adc84aa08cdd21b9f6d1a64addd92ae60b36f02d0521ed305aa4086"),
}

# The template files the exporting checks use: Linux, Windows and macOS desktop presets.
TEMPLATE_FILES = (
    "version.txt",
    "linux_release.x86_64",
    "linux_debug.x86_64",
    "windows_release_x86_64.exe",
    "windows_debug_x86_64.exe",
    "macos.zip",
)


def host_platform() -> str:
    machine = platform.machine().lower()
    if sys.platform.startswith("linux") and machine in ("x86_64", "amd64"):
        return "linux-x86_64"
    if sys.platform == "darwin":
        return "macos-universal"
    if sys.platform == "win32" and machine in ("x86_64", "amd64"):
        return "windows-x86_64"
    raise SystemExit(f"platform_matrix: no locked editor build runs on {sys.platform}/{machine}")


def editor_in(directory: Path, host: str, version: str) -> Path:
    if host == "linux-x86_64":
        return directory / f"Godot_v{version}-stable_linux.x86_64"
    if host == "macos-universal":
        return directory / "Godot.app" / "Contents" / "MacOS" / "Godot"
    # The windowed executable writes nothing to a redirected stdout, so its
    # `--version` reads as empty; the console executable beside it relays the
    # engine's output, and is what a command-line client drives on Windows.
    return directory / f"Godot_v{version}-stable_win64_console.exe"


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(url: str, sha256: str, name: str, cache: Path) -> Path:
    """The verified artifact, from a cache keyed by its digest or downloaded into it."""
    target = cache / sha256 / name
    if target.is_file() and sha256_of(target) == sha256:
        return target
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_name(target.name + ".partial")
    with urllib.request.urlopen(url, timeout=120) as response, partial.open("wb") as out:
        shutil.copyfileobj(response, out, 1 << 20)
    actual = sha256_of(partial)
    if actual != sha256:
        partial.unlink()
        raise RuntimeError(f"{name}: downloaded SHA-256 {actual} is not the locked {sha256}")
    partial.replace(target)
    return target


def extract(archive: Path, directory: Path, members: tuple[str, ...] | None = None) -> None:
    """Extract keeping executable bits: `unzip` where it exists, else zipfile plus modes."""
    directory.mkdir(parents=True, exist_ok=True)
    unzip = shutil.which("unzip") if sys.platform != "win32" else None
    if unzip:
        command = [unzip, "-q", "-o", str(archive)]
        if members:
            command += list(members)
        command += ["-d", str(directory)]
        subprocess.run(command, check=True)
        return
    with zipfile.ZipFile(archive) as bundle:
        for info in bundle.infolist():
            if members and info.filename not in members:
                continue
            written = Path(bundle.extract(info, directory))
            mode = (info.external_attr >> 16) & 0o777
            if mode and sys.platform != "win32":
                written.chmod(mode)


def artifact(release: dict, host: str, kind: str) -> dict:
    for candidate in release["artifacts"]:
        if candidate["artifact_kind"] == kind and candidate["platform"] in (host, "all-platforms"):
            return candidate
    raise SystemExit(f"platform_matrix: {release['version']} locks no {kind} for {host}")


def editor(release: dict, host: str, cache: Path, work: Path) -> Path:
    locked = artifact(release, host, "editor")
    archive = fetch(locked["download_url"], locked["sha256"], locked["artifact_name"], cache)
    directory = work / "godot" / release["version"]
    binary = editor_in(directory, host, release["version"])
    if not binary.is_file():
        extract(archive, directory)
    if host == "macos-universal" and shutil.which("xattr"):
        # A downloaded app keeps no quarantine attribute from urllib, but a cache
        # filled by a browser would; Gatekeeper must not stop a headless check.
        subprocess.run(["xattr", "-dr", "com.apple.quarantine", str(directory / "Godot.app")],
                       check=False, stderr=subprocess.DEVNULL)
    if not binary.is_file():
        raise RuntimeError(f"{locked['artifact_name']} holds no {binary.name}")
    return binary


def install_templates(release: dict, cache: Path, data_home: Path) -> Path:
    version = release["version"]
    label = f"{version}.stable" if not version.endswith(".0") else f"{version[:-2]}.stable"
    installed = templates_root(data_home) / label
    if all((installed / name).is_file() for name in TEMPLATE_FILES):
        return installed
    locked = artifact(release, "all-platforms", "export_templates")
    archive = fetch(locked["download_url"], locked["sha256"], locked["artifact_name"], cache)
    staging = data_home / ".staging" / label
    shutil.rmtree(staging, ignore_errors=True)
    extract(archive, staging, tuple(f"templates/{name}" for name in TEMPLATE_FILES))
    installed.mkdir(parents=True, exist_ok=True)
    for name in TEMPLATE_FILES:
        shutil.move(str(staging / "templates" / name), installed / name)
    shutil.rmtree(data_home / ".staging", ignore_errors=True)
    return installed


def templates_root(data_home: Path) -> Path:
    """Where the server, run with `data_environment`, finds export templates."""
    if sys.platform == "darwin":
        return data_home / "Library" / "Application Support" / "Godot" / "export_templates"
    if sys.platform == "win32":
        return data_home / "Godot" / "export_templates"
    return data_home / "godot" / "export_templates"


def data_environment(data_home: Path) -> dict:
    """The variable each host's Godot, and so the server, reads its data directory from."""
    if sys.platform == "darwin":
        return {"HOME": str(data_home)}
    if sys.platform == "win32":
        return {"APPDATA": str(data_home)}
    return {"XDG_DATA_HOME": str(data_home)}


def gut_archive(version: str, cache: Path) -> Path:
    gut, sha256 = GUT[".".join(version.split(".")[:2])]
    url = f"https://github.com/bitwes/Gut/archive/refs/tags/v{gut}.zip"
    return fetch(url, sha256, f"Gut-v{gut}.zip", cache)


def run_check(check: str, binary: Path, godot: Path, version: str, cache: Path, out: Path,
              environment: dict) -> dict:
    script = REPO / "scripts" / "real_client" / (
        "e2e_complete.py" if check == "e2e" else f"{check}_smoke.py")
    command = [sys.executable, "-I", "-B", str(script), "--binary", str(binary), "--godot", str(godot)]
    if check == "e2e":
        command += ["--gut", str(gut_archive(version, cache))]
    if check == "m6":
        command += ["--require-templates"]
    started = time.monotonic()
    log = out / f"{check}-{version}.txt"
    with log.open("wb") as captured:
        try:
            completed = subprocess.run(command, stdout=captured, stderr=subprocess.STDOUT,
                                       env=environment, timeout=DEADLINE_SECONDS, check=False)
            code = completed.returncode
        except subprocess.TimeoutExpired:
            code = "timeout"
    return {"check": check, "version": version, "exit": code,
            "seconds": round(time.monotonic() - started), "log": log.name}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--versions", default="")
    parser.add_argument("--checks", default=",".join(CHECKS))
    parser.add_argument("--cache", type=Path, default=Path(
        os.environ.get("GODOT_ARTIFACT_CACHE", Path.home() / ".cache" / "godot-mcp" / "godot-artifacts")))
    parser.add_argument("--data-home", type=Path)
    parser.add_argument("--work", type=Path, help="where builds are extracted (default: <out>/work)")
    arguments = parser.parse_args()

    binary = arguments.binary.resolve()
    host = host_platform()
    releases = json.loads(LOCK.read_text(encoding="utf-8"))["releases"]
    wanted = [v for v in arguments.versions.split(",") if v] or [r["version"] for r in releases]
    checks = [c for c in arguments.checks.split(",") if c]
    unknown = sorted(set(checks) - set(CHECKS))
    if unknown:
        raise SystemExit(f"platform_matrix: unknown checks {unknown}; known: {', '.join(CHECKS)}")
    out = arguments.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    work = (arguments.work or out / "work").resolve()
    # An export check without this build's templates proves only the refusal,
    # so exporting checks always get them, in a private data home by default.
    exporting = any(check in EXPORTING for check in checks)
    data_home = arguments.data_home or (work / "data-home" if exporting else None)
    environment = dict(os.environ)
    if data_home:
        data_home.mkdir(parents=True, exist_ok=True)
        environment.update(data_environment(data_home.resolve()))

    results = []
    for version in wanted:
        release = next((r for r in releases if r["version"] == version), None)
        if release is None:
            raise SystemExit(f"platform_matrix: {version} is not a locked release")
        try:
            godot = editor(release, host, arguments.cache, work)
            if data_home and exporting:
                install_templates(release, arguments.cache, data_home.resolve())
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            results += [{"check": check, "version": version, "exit": "not started",
                         "reason": str(error)} for check in checks]
            continue
        for check in checks:
            result = run_check(check, binary, godot, version, arguments.cache, out, environment)
            results.append(result)
            print(json.dumps(result), flush=True)

    summary = {
        "host": host,
        "binary_sha256": sha256_of(binary),
        "results": results,
        "passed": sum(1 for result in results if result["exit"] == 0),
        "total": len(results),
    }
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: summary[key] for key in ("host", "passed", "total")}))
    return 0 if summary["passed"] == summary["total"] else 1


if __name__ == "__main__":
    sys.exit(main())
