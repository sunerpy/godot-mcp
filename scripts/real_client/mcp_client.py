"""A thin wrapper over the official MCP Python SDK for real-entry smokes.

Every smoke in this directory drives the *built* `godot-mcp` binary over stdio
through `mcp.client.stdio` and `mcp.ClientSession`, exactly as an MCP client
does; `connect_http` drives the same binary over its authenticated
`trusted_local` Streamable HTTP transport through `mcp.client.streamable_http`.
Nothing here reaches into the server: a result is only what the client
receives.

`GODOT_MCP_SMOKE_BOUNDARY=cgroup` runs every server this module starts inside
a delegated user scope (`systemd-run --user --scope -p Delegate=yes`) with
`[process] boundary = "cgroup"`, so any smoke can be repeated with every Godot
launch in its own launch cgroup. `GODOT_MCP_SMOKE_BOUNDARY=adapter` runs them in
such a scope too, with the project untrusted and every launch inside the
reference namespace adapter (`scripts/adapters/linux-namespace-adapter.sh`), whose
boundaries live in one more delegated scope this module holds open for the run.
The variable is read here only; servers never see it.
"""

from __future__ import annotations

import asyncio
import atexit
import contextlib
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator

from mcp import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client


@dataclass
class ToolResult:
    """One `tools/call` answer: the structured payload and the error flag."""

    name: str
    is_error: bool
    payload: Any
    raw_text: str

    def require_ok(self) -> Any:
        if self.is_error:
            raise AssertionError(f"{self.name} failed: {self.raw_text}")
        return self.payload


@dataclass
class Server:
    """Where and how to start one `godot-mcp serve --stdio` process.

    Exactly one of `project` (start bound) and `workspace` (start in bootstrap
    state) is given.
    """

    binary: Path
    project: Path | None = None
    profile: str = "full"
    godot: Path | None = None
    cwd: Path | None = None
    extra_config: dict[str, Any] = field(default_factory=dict)
    workspace: Path | None = None


def private_godot_environment(home: Path) -> dict:
    """The environment for a Godot a smoke launches itself, kept in `home`.

    Godot keeps its editor settings and user data under `HOME` on Linux and
    macOS and under the profile directories on Windows, which also cannot run a
    program (Winsock, the DLL search) without its system variables.
    """
    home.mkdir(parents=True, exist_ok=True)
    if sys.platform != "win32":
        return {"HOME": str(home), "PATH": "/usr/bin:/bin"}
    keep = ("SystemRoot", "SystemDrive", "windir", "COMSPEC", "PATHEXT", "OS",
            "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE")
    environment = {name: os.environ[name] for name in keep if name in os.environ}
    for name, below in (("USERPROFILE", ""), ("APPDATA", "AppData/Roaming"),
                        ("LOCALAPPDATA", "AppData/Local"), ("TEMP", "Temp"), ("TMP", "Temp")):
        directory = home / below if below else home
        directory.mkdir(parents=True, exist_ok=True)
        environment[name] = str(directory)
    environment["PATH"] = str(Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32")
    return environment


def _boundary() -> str | None:
    """The process boundary the smoke run asks for, if any."""
    boundary = os.environ.get("GODOT_MCP_SMOKE_BOUNDARY")
    if boundary not in (None, "", "cgroup", "adapter"):
        raise ValueError(f"GODOT_MCP_SMOKE_BOUNDARY={boundary!r}: only cgroup or adapter is supported")
    return boundary or None


_ADAPTER_ROOT: Path | None = None


def _adapter_root() -> Path:
    """A delegated cgroup the namespace adapter makes its boundaries in: a user
    scope whose own process waits in a leaf beside them, held for the whole run."""
    global _ADAPTER_ROOT
    if _ADAPTER_ROOT is not None:
        return _ADAPTER_ROOT
    holder = subprocess.Popen(
        ["systemd-run", "--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes", "--",
         "sleep", "infinity"])
    atexit.register(lambda: (holder.kill(), holder.wait()))
    deadline = time.monotonic() + 10
    while True:
        membership = Path(f"/proc/{holder.pid}/cgroup").read_text().strip()
        cgroup = membership.split("::", 1)[1] if "::" in membership else ""
        program = Path(f"/proc/{holder.pid}/cmdline").read_bytes().split(b"\0")[0]
        # systemd-run execs the program itself once its scope exists.
        if cgroup.endswith(".scope") and program.endswith(b"sleep"):
            break
        if time.monotonic() > deadline:
            raise RuntimeError("the adapter's delegated scope never started")
        time.sleep(0.02)
    root = Path("/sys/fs/cgroup") / cgroup.lstrip("/")
    (root / "holder").mkdir()
    (root / "holder" / "cgroup.procs").write_text(str(holder.pid))
    _ADAPTER_ROOT = root
    return root


def _launch_prefix() -> list[str]:
    """What a server command is wrapped in for the selected boundary. cgroup v2
    moves a process between cgroups only inside the user's own delegated tree,
    so the adapter's server runs in a delegated scope too."""
    if _boundary() in ("cgroup", "adapter"):
        return ["systemd-run", "--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes", "--"]
    return []


def _process_section() -> list[str]:
    if _boundary() == "cgroup":
        return ["", "[process]", 'boundary = "cgroup"']
    if _boundary() == "adapter":
        adapter = Path(__file__).resolve().parents[1] / "adapters" / "linux-namespace-adapter.sh"
        return ["", "[process]", 'boundary = "adapter"', 'trust = "untrusted"', "",
                "[process.adapter]", f"program = {json.dumps(str(adapter))}",
                f"root = {json.dumps(str(_adapter_root()))}"]
    return []


def _config_text(server: Server) -> str:
    lines = ['transport = "stdio"']
    if server.godot is not None:
        lines.append(f"godot_binary = {json.dumps(str(server.godot))}")
    for key, value in server.extra_config.items():
        lines.append(f"{key} = {json.dumps(value)}")
    lines.extend(_process_section())
    return "\n".join(lines) + "\n"


@contextlib.asynccontextmanager
async def connect(server: Server) -> AsyncIterator["Client"]:
    """Start the server, initialize an MCP session and yield a client."""
    with tempfile.TemporaryDirectory(prefix="godot-mcp-client-") as scratch:
        config = Path(scratch) / "serve.toml"
        config.write_text(_config_text(server), encoding="utf-8")
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GODOT_MCP_")
        }
        if (server.project is None) == (server.workspace is None):
            raise ValueError("start a server with exactly one of project and workspace")
        target = (
            ["--project", str(server.project)]
            if server.project is not None
            else ["--workspace", str(server.workspace)]
        )
        command = [
            *_launch_prefix(),
            str(server.binary),
            "serve",
            "--stdio",
            "--config",
            str(config),
            *target,
            "--profile",
            server.profile,
        ]
        parameters = StdioServerParameters(
            command=command[0],
            args=command[1:],
            env=environment,
            cwd=str(server.cwd) if server.cwd is not None else None,
        )
        kept = os.environ.get("GODOT_MCP_SMOKE_STDERR")
        stderr_path = Path(kept) if kept else Path(scratch) / "server.stderr"
        with stderr_path.open("a", encoding="utf-8") as errlog:
            notifications: list[str] = []

            async def record(message: Any) -> None:
                root = getattr(message, "root", None)
                method = getattr(root, "method", None)
                if isinstance(method, str):
                    notifications.append(method)

            async with stdio_client(parameters, errlog=errlog) as (read, write):
                async with ClientSession(read, write, message_handler=record) as session:
                    initialized = await session.initialize()
                    client = Client(session, initialized, stderr_path)
                    client.notifications = notifications
                    yield client


def _loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def _direct_http_client(headers=None, timeout=None, auth=None):
    """An httpx client that ignores proxy settings: the server is on loopback."""
    import httpx

    return httpx.AsyncClient(
        headers=headers,
        timeout=timeout if timeout is not None else httpx.Timeout(30.0, read=300.0),
        auth=auth,
        follow_redirects=True,
        trust_env=False,
    )


class HttpServer:
    """One `godot-mcp serve` process on authenticated `trusted_local` HTTP."""

    def __init__(self, endpoint: str, token: str, stderr: Path) -> None:
        self.endpoint = endpoint
        self.token = token
        self.stderr = stderr

    @contextlib.asynccontextmanager
    async def session(self) -> AsyncIterator["Client"]:
        """Open one more MCP session, authenticated with this launch's secret."""
        from mcp.client.streamable_http import streamablehttp_client

        headers = {"Authorization": f"Bearer {self.token}"}
        async with streamablehttp_client(
            self.endpoint, headers=headers, httpx_client_factory=_direct_http_client
        ) as (read, write, _session_id):
            async with ClientSession(read, write) as session:
                initialized = await session.initialize()
                yield Client(session, initialized, self.stderr)


@contextlib.asynccontextmanager
async def connect_http(server: Server, timeout: float = 120.0) -> AsyncIterator[HttpServer]:
    """Start the server on `trusted_local` HTTP, wait until it publishes its
    endpoint and launch secret, and yield a handle that opens sessions."""
    if server.project is None:
        raise ValueError("an HTTP server starts bound to a project")
    port = _loopback_port()
    lines = []
    if server.godot is not None:
        lines.append(f"godot_binary = {json.dumps(str(server.godot))}")
    for key, value in server.extra_config.items():
        lines.append(f"{key} = {json.dumps(value)}")
    config_text = "\n".join([
        'transport = "http"',
        *lines,
        *_process_section(),
        "",
        "[http]",
        f'bind = "127.0.0.1:{port}"',
        'mode = "trusted_local"',
        f'host = "127.0.0.1:{port}"',
        f'origin = "http://127.0.0.1:{port}"',
        "",
        "[http.local_secret.env]",
        'var = "GODOT_MCP_SMOKE_LOCAL_SECRET"',
        "",
    ])
    with tempfile.TemporaryDirectory(prefix="godot-mcp-http-") as scratch:
        config = Path(scratch) / "serve.toml"
        config.write_text(config_text, encoding="utf-8")
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GODOT_MCP_")
        }
        environment["GODOT_MCP_SMOKE_LOCAL_SECRET"] = "config-indirection-is-not-the-launch-secret"
        kept = os.environ.get("GODOT_MCP_SMOKE_STDERR")
        stderr_path = Path(kept) if kept else Path(scratch) / "server.stderr"
        process = await asyncio.create_subprocess_exec(
            *_launch_prefix(), str(server.binary), "serve", "--config", str(config),
            "--project", str(server.project), "--profile", server.profile,
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE, env=environment,
            cwd=str(server.cwd) if server.cwd is not None else None,
        )
        endpoint = secret = None
        with stderr_path.open("a", encoding="utf-8") as errlog:

            async def announced() -> None:
                nonlocal endpoint, secret
                while endpoint is None or secret is None:
                    raw = await process.stderr.readline()
                    if not raw:
                        raise AssertionError(f"the HTTP server exited before serving ({stderr_path})")
                    line = raw.decode("utf-8", "replace")
                    errlog.write(line)
                    if "HTTP listening at " in line:
                        endpoint = line.split("HTTP listening at ", 1)[1].strip()
                    if "launch secret published at " in line:
                        secret = Path(line.split("launch secret published at ", 1)[1].strip())

            async def drain() -> None:
                while raw := await process.stderr.readline():
                    errlog.write(raw.decode("utf-8", "replace"))

            drained = None
            try:
                await asyncio.wait_for(announced(), timeout)
                drained = asyncio.create_task(drain())
                token = secret.read_text(encoding="utf-8").strip()
                yield HttpServer(endpoint, token, stderr_path)
            finally:
                if process.returncode is None:
                    if sys.platform == "win32":
                        process.terminate()
                    else:
                        process.send_signal(signal.SIGINT)
                    try:
                        await asyncio.wait_for(process.wait(), 30)
                    except asyncio.TimeoutError:
                        process.kill()
                        await process.wait()
                if drained is not None:
                    await drained


class Client:
    def __init__(self, session: ClientSession, initialized: Any, stderr: Path) -> None:
        self.session = session
        self.initialized = initialized
        self.stderr = stderr
        self.transcript: list[str] = []
        self.notifications: list[str] = []

    async def tool_names(self) -> list[str]:
        listed = await self.session.list_tools()
        return sorted(tool.name for tool in listed.tools)

    async def tools(self) -> list[Any]:
        listed = await self.session.list_tools()
        return list(listed.tools)

    async def call(self, name: str, arguments: dict[str, Any] | None = None) -> ToolResult:
        result = await self.session.call_tool(name, arguments or {})
        text = "".join(
            getattr(item, "text", "") for item in result.content if getattr(item, "type", "") == "text"
        )
        payload = result.structuredContent
        if payload is None and text:
            try:
                payload = json.loads(text)
            except json.JSONDecodeError:
                payload = text
        raw = json.dumps(payload, sort_keys=True) if payload is not None else text
        self.transcript.append(raw)
        return ToolResult(name=name, is_error=bool(result.isError), payload=payload, raw_text=raw)

    async def read_resource(self, uri: str) -> Any:
        result = await self.session.read_resource(uri)
        self.transcript.append(str(result))
        return result
