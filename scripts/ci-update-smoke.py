#!/usr/bin/env python3
"""Release-gate: prove this build can self-update and come back running.

There is never a "next" GitHub release to update *to* while cutting this one,
so the smoke serves the just-built artifact as a fake newer version (99.0.0)
from a loopback mock of Scalattice Cloud, then flips `/latest` back to the
real version so the restarted agent does not loop.

It vets both paths that can brick a fleet:

  live/remote  — mock WS sends ``{"type":"control","action":"update"}`` to the
                 running foreground job (the macOS launchd-suicide bug)
  CLI          — ``scalattice-agent update`` (daily timer / manual)

Failing this step must block uploading the GitHub release asset.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import platform
import shutil
import socket
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Optional

FAKE_VERSION = "99.0.0"
FAKE_TAG = "v99.0.0"
SMOKE_TOKEN = "slt_provider_ci_update_smoke"
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
WIN_RUN_KEY = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run"
WIN_RUN_VALUES = ("ScalatticeAgent", "ScalatticeAgentTray")
WIN_APP_ID = "A4E8B2C1-9F3D-4A6E-8B1C-2D5E7F9A0B3C"


class Fail(Exception):
    pass


class MockState:
    def __init__(self, asset: Path, asset_name: str, real_version: str, digest: str):
        self.lock = threading.Lock()
        self.asset = asset
        self.asset_name = asset_name
        self.real_version = real_version
        self.digest = digest
        self.advertised_version = FAKE_VERSION
        self.advertised_tag = FAKE_TAG
        self.downloads = 0
        self.registers = 0
        self.control_acks = 0
        self.live: list["WsConn"] = []
        self.download_event = threading.Event()
        self.register_event = threading.Event()
        self.ack_event = threading.Event()

    def latest_payload(self) -> bytes:
        with self.lock:
            version = self.advertised_version
            tag = self.advertised_tag
            name = self.asset_name
            digest = self.digest
        body = {
            "tag": tag,
            "version": version,
            "checksums": {name: digest},
        }
        return json.dumps(body).encode()

    def note_download(self) -> None:
        with self.lock:
            self.downloads += 1
            self.advertised_version = self.real_version
            self.advertised_tag = f"v{self.real_version}"
        self.download_event.set()

    def reset_advertised(self) -> None:
        with self.lock:
            self.advertised_version = FAKE_VERSION
            self.advertised_tag = FAKE_TAG
        self.download_event.clear()

    def note_register(self) -> None:
        with self.lock:
            self.registers += 1
        self.register_event.set()

    def note_ack(self) -> None:
        with self.lock:
            self.control_acks += 1
        self.ack_event.set()

    def send_control(self, action: str) -> int:
        payload = json.dumps({"type": "control", "action": action}).encode()
        with self.lock:
            conns = list(self.live)
        sent = 0
        for conn in conns:
            try:
                conn.send_text(payload)
                sent += 1
            except OSError:
                pass
        return sent

    def snapshot(self) -> dict:
        with self.lock:
            return {
                "downloads": self.downloads,
                "registers": self.registers,
                "control_acks": self.control_acks,
                "advertised": self.advertised_version,
                "live": len(self.live),
            }


class WsConn:
    def __init__(self, sock: socket.socket, state: MockState):
        self.sock = sock
        self.state = state

    def send_text(self, payload: bytes) -> None:
        ws_send(self.sock, payload, opcode=1)

    def close(self) -> None:
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def ws_accept_key(key: str) -> str:
    digest = hashlib.sha1((key.strip() + WS_GUID).encode("ascii")).digest()
    return base64.b64encode(digest).decode("ascii")


def ws_send(sock: socket.socket, payload: bytes, opcode: int = 1) -> None:
    header = bytearray()
    header.append(0x80 | (opcode & 0x0F))
    n = len(payload)
    if n < 126:
        header.append(n)
    elif n < 65536:
        header.append(126)
        header.extend(struct.pack("!H", n))
    else:
        header.append(127)
        header.extend(struct.pack("!Q", n))
    sock.sendall(header + payload)


def ws_read_frame(sock: socket.socket) -> Optional[tuple[int, bytes]]:
    hdr = _recv_exact(sock, 2)
    if hdr is None:
        return None
    opcode = hdr[0] & 0x0F
    masked = bool(hdr[1] & 0x80)
    length = hdr[1] & 0x7F
    if length == 126:
        ext = _recv_exact(sock, 2)
        if ext is None:
            return None
        length = struct.unpack("!H", ext)[0]
    elif length == 127:
        ext = _recv_exact(sock, 8)
        if ext is None:
            return None
        length = struct.unpack("!Q", ext)[0]
    mask = b""
    if masked:
        got = _recv_exact(sock, 4)
        if got is None:
            return None
        mask = got
    data = _recv_exact(sock, length) if length else b""
    if data is None:
        return None
    if masked:
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
    return opcode, data


def _recv_exact(sock: socket.socket, n: int) -> Optional[bytes]:
    buf = bytearray()
    while len(buf) < n:
        try:
            chunk = sock.recv(n - len(buf))
        except OSError:
            return None
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def handle_ws_client(sock: socket.socket, state: MockState) -> None:
    sock.settimeout(120)
    conn = WsConn(sock, state)
    try:
        raw = _recv_http_headers(sock)
        if raw is None:
            return
        key = ""
        for line in raw.split("\r\n"):
            if line.lower().startswith("sec-websocket-key:"):
                key = line.split(":", 1)[1].strip()
        if not key:
            sock.sendall(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            return
        accept = ws_accept_key(key)
        sock.sendall(
            (
                "HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Accept: {accept}\r\n"
                "\r\n"
            ).encode("ascii")
        )
        ready = {
            "type": "ready",
            "nodeId": "smoke-node",
            "catalog": [],
            "computeDevices": [],
            "enabledModels": [],
            "schedule": {"acceptingJobs": False},
        }
        conn.send_text(json.dumps(ready).encode())
        with state.lock:
            state.live.append(conn)
        while True:
            frame = ws_read_frame(sock)
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 8:
                break
            if opcode == 9:
                ws_send(sock, payload, opcode=10)
                continue
            if opcode != 1:
                continue
            try:
                msg = json.loads(payload.decode("utf-8", errors="replace"))
            except json.JSONDecodeError:
                continue
            kind = msg.get("type") or msg.get("kind")
            if kind == "register":
                state.note_register()
                registered = {
                    "type": "registered",
                    "nodeId": "smoke-node",
                    "models": msg.get("models") or [],
                }
                conn.send_text(json.dumps(registered).encode())
            elif kind == "control_ack":
                state.note_ack()
            elif kind == "heartbeat":
                pong = {
                    "type": "pong",
                    "computeDevices": [],
                    "enabledModels": [],
                    "purgeModels": [],
                    "schedule": {"acceptingJobs": False},
                }
                conn.send_text(json.dumps(pong).encode())
    except OSError:
        pass
    finally:
        with state.lock:
            if conn in state.live:
                state.live.remove(conn)
        conn.close()


def _recv_http_headers(sock: socket.socket) -> Optional[str]:
    buf = bytearray()
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            return None
        buf.extend(chunk)
        if len(buf) > 64 * 1024:
            return None
    return buf.split(b"\r\n\r\n", 1)[0].decode("iso-8859-1", errors="replace")


def start_ws_server(state: MockState) -> tuple[threading.Thread, int]:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", 0))
    sock.listen(16)
    port = sock.getsockname()[1]
    stop = threading.Event()

    def loop() -> None:
        sock.settimeout(0.5)
        while not stop.is_set():
            try:
                client, _ = sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(
                target=handle_ws_client, args=(client, state), daemon=True
            ).start()
        try:
            sock.close()
        except OSError:
            pass

    thread = threading.Thread(target=loop, daemon=True)
    thread.start()
    thread.stop_event = stop  # type: ignore[attr-defined]
    return thread, port


def make_http_handler(state: MockState):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt: str, *args) -> None:  # noqa: A003
            sys.stderr.write("[mock-http] " + (fmt % args) + "\n")

        def do_GET(self) -> None:  # noqa: N802
            path = self.path.split("?", 1)[0]
            if path.endswith("/latest") or path == "/latest":
                body = state.latest_payload()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if "/download/" in path:
                name = path.rstrip("/").rsplit("/", 1)[-1]
                if name != state.asset_name:
                    self.send_error(404, f"unknown asset {name}")
                    return
                size = state.asset.stat().st_size
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(size))
                self.end_headers()
                with state.asset.open("rb") as f:
                    shutil.copyfileobj(f, self.wfile)
                state.note_download()
                return
            self.send_error(404, "not found")

    return Handler


def wait_until(desc: str, timeout: float, fn) -> None:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            if fn():
                print(f"==> ok: {desc}")
                return
        except Exception as err:  # noqa: BLE001
            last = err
        time.sleep(0.4)
    extra = f" ({last})" if last else ""
    raise Fail(f"timeout waiting for {desc}{extra}")


def run_agent(bin_path: Path, args: list[str], env: dict, timeout: int = 180, quiet: bool = False) -> subprocess.CompletedProcess:
    if not quiet:
        print(f"==> {' '.join([str(bin_path), *args])}")
    return subprocess.run(
        [str(bin_path), *args],
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )


def print_cmd(result: subprocess.CompletedProcess) -> None:
    if result.stdout:
        sys.stdout.write(result.stdout)
        if not result.stdout.endswith("\n"):
            sys.stdout.write("\n")
    if result.stderr:
        sys.stderr.write(result.stderr)
        if not result.stderr.endswith("\n"):
            sys.stderr.write("\n")


def detect_assets(dist: Path) -> tuple[Path, Optional[Path]]:
    system = sys.platform
    machine = platform.machine().lower()
    if system.startswith("linux"):
        arch = "aarch64" if machine in ("aarch64", "arm64") else "x86_64"
        name = f"scalattice-agent-{arch}-unknown-linux-gnu.tar.gz"
        asset = dist / name
        if not asset.is_file():
            raise Fail(f"missing {asset}")
        return asset, None
    if system == "darwin":
        asset = dist / "scalattice-agent-aarch64-apple-darwin.tar.gz"
        if not asset.is_file():
            raise Fail(f"missing {asset}")
        return asset, None
    if system in ("win32", "cygwin"):
        setup = dist / "ScalatticeAgentSetup-x86_64.exe"
        zipped = dist / "scalattice-agent-x86_64-pc-windows-msvc.zip"
        if not setup.is_file():
            raise Fail(f"missing {setup}")
        if not zipped.is_file():
            raise Fail(f"missing {zipped} (used to seed the install before the installer-based update)")
        return setup, zipped
    raise Fail(f"unsupported platform {system}/{machine}")


def extract_unix(archive: Path, dest: Path) -> None:
    dest.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:gz") as tar:
        tar.extractall(dest)


def extract_zip(archive: Path, dest: Path) -> None:
    dest.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive) as zf:
        zf.extractall(dest)


def find_cuda_stub() -> Optional[Path]:
    """NVIDIA driver (`libcuda.so.1`) is not in the release tarball and is not
    present on GitHub-hosted runners. The CUDA *toolkit* ships a link stub that
    is enough for the process to start (inference will not use a GPU here)."""
    names = ("libcuda.so.1", "libcuda.so")
    roots: list[Path] = []
    for key in ("CUDA_PATH", "CUDA_HOME"):
        raw = os.environ.get(key, "").strip()
        if raw:
            roots.append(Path(raw))
    if Path("/usr/local").is_dir():
        roots.extend(sorted(Path("/usr/local").glob("cuda*")))
    subdirs = (
        Path("lib64/stubs"),
        Path("lib/stubs"),
        Path("targets/aarch64-linux/lib/stubs"),
        Path("targets/sbsa-linux/lib/stubs"),
        Path("targets/x86_64-linux/lib/stubs"),
        Path("lib64"),
        Path("lib"),
    )
    dirs: list[Path] = []
    for root in roots:
        dirs.extend(root / sub for sub in subdirs)
    dirs.extend(
        [
            Path("/usr/lib/wsl/lib"),
            Path("/usr/lib/aarch64-linux-gnu"),
            Path("/usr/lib/x86_64-linux-gnu"),
        ]
    )
    for directory in dirs:
        for name in names:
            path = directory / name
            if path.is_file():
                return path.resolve()
    for root in roots:
        if not root.is_dir():
            continue
        for pattern in ("**/stubs/libcuda.so.1", "**/stubs/libcuda.so"):
            hit = next((p for p in root.glob(pattern) if p.is_file()), None)
            if hit is not None:
                return hit.resolve()
    return None


def compile_dummy_libcuda(lib_dir: Path) -> Path:
    """Satisfy DT_NEEDED libcuda.so.1 so --version / foreground can start."""
    dest = lib_dir / "libcuda.so.1"
    cc = shutil.which("gcc") or shutil.which("cc")
    if cc is None:
        raise Fail(
            "GPU-linked agent needs libcuda.so.1 to start, and this runner has "
            "neither an NVIDIA driver, a CUDA toolkit stub, nor gcc to build a dummy."
        )
    src = lib_dir / ".scalattice_libcuda_stub.c"
    src.write_text(
        "/* CI-only loader stub; not a real NVIDIA driver. */\n"
        "void scalattice_cuda_ci_stub(void) {}\n",
        encoding="utf-8",
    )
    try:
        compiled = subprocess.run(
            [cc, "-shared", "-fPIC", "-Wl,-soname,libcuda.so.1", "-o", str(dest), str(src)],
            capture_output=True,
            text=True,
            check=False,
        )
        if compiled.returncode != 0 or not dest.is_file():
            detail = (compiled.stderr or compiled.stdout or "").strip()
            raise Fail(f"failed to compile dummy libcuda.so.1: {detail or compiled.returncode}")
    finally:
        src.unlink(missing_ok=True)
    return dest


def install_cuda_driver_stub(lib_dir: Path) -> None:
    if not sys.platform.startswith("linux"):
        return
    dest = lib_dir / "libcuda.so.1"
    if dest.is_file():
        return
    lib_dir.mkdir(parents=True, exist_ok=True)
    stub = find_cuda_stub()
    if stub is not None:
        try:
            dest.symlink_to(stub)
        except OSError:
            shutil.copy2(stub, dest)
        print(f"==> CUDA driver stub {stub} -> {dest} (CPU-only runner)")
        return
    compiled = compile_dummy_libcuda(lib_dir)
    print(f"==> compiled dummy {compiled} (no NVIDIA driver or CUDA stub on this runner)")


def seed_unix(staging: Path, home: Path) -> Path:
    src = staging / "scalattice-agent"
    if not src.is_file():
        raise Fail(f"archive missing scalattice-agent at {src}")
    bindir = home / ".local" / "bin"
    bindir.mkdir(parents=True, exist_ok=True)
    dest = bindir / "scalattice-agent"
    shutil.copy2(src, dest)
    dest.chmod(0o755)
    lib_dest = home / ".local" / "lib" / "scalattice"
    lib_dest.mkdir(parents=True, exist_ok=True)
    lib_src = staging / "lib"
    if lib_src.is_dir():
        for item in lib_src.iterdir():
            if item.is_file():
                shutil.copy2(item, lib_dest / item.name)
    install_cuda_driver_stub(lib_dest)
    return dest


def seed_windows(staging: Path, localappdata: Path) -> Path:
    exe = None
    for candidate in staging.rglob("scalattice-agent.exe"):
        exe = candidate
        break
    if exe is None:
        raise Fail("zip missing scalattice-agent.exe")
    bindir = localappdata / "Scalattice" / "bin"
    libdir = localappdata / "Scalattice" / "lib"
    bindir.mkdir(parents=True, exist_ok=True)
    dest = bindir / "scalattice-agent.exe"
    shutil.copy2(exe, dest)
    lib_src = exe.parent / "lib"
    if not lib_src.is_dir():
        lib_src = staging / "lib"
    if lib_src.is_dir():
        libdir.mkdir(parents=True, exist_ok=True)
        for item in lib_src.iterdir():
            if item.is_file():
                shutil.copy2(item, libdir / item.name)
    for helper in ("scalattice-run.cmd", "launch-background.vbs", "launch-tray.vbs"):
        src = exe.parent / helper
        if src.is_file():
            shutil.copy2(src, bindir / helper)
    return dest


def agent_version(bin_path: Path, env: dict) -> str:
    result = run_agent(bin_path, ["--version"], env, timeout=30)
    print_cmd(result)
    if result.returncode != 0:
        raise Fail(f"--version failed ({result.returncode})")
    text = (result.stdout or result.stderr or "").strip().split()
    if not text:
        raise Fail("empty --version output")
    return text[-1].lstrip("v")


def write_agent_env(home: Path, extra: dict[str, str]) -> Path:
    cfg = home / ".config" / "scalattice"
    cfg.mkdir(parents=True, exist_ok=True)
    path = cfg / "agent.env"
    lines = [f"{k}={v}" for k, v in extra.items()]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def status_running(bin_path: Path, env: dict) -> bool:
    result = run_agent(bin_path, ["status"], env, timeout=30, quiet=True)
    text = f"{result.stdout}\n{result.stderr}"
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("Service") and "running" in stripped and "not " not in stripped:
            return True
        if stripped.startswith("Agent") and stripped.endswith("running"):
            return True
    return False


def log_paths(home: Path, localappdata: Optional[Path]) -> list[Path]:
    paths = [
        home / ".local" / "share" / "scalattice" / "agent.log",
        home / ".local" / "share" / "scalattice" / "launchd.err.log",
        home / ".local" / "share" / "scalattice" / "launchd.out.log",
        home / "Library" / "Logs" / "agent.log",
    ]
    if localappdata is not None:
        paths.append(localappdata / "Scalattice" / "logs" / "agent.log")
    return paths


def dump_logs(home: Path, localappdata: Optional[Path]) -> None:
    print("==> agent logs (tail)")
    for path in log_paths(home, localappdata):
        if not path.is_file():
            continue
        print(f"---- {path} ----")
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError as err:
            print(f"  (unreadable: {err})")
            continue
        for line in lines[-80:]:
            print(line)


def ensure_linux_user_systemd() -> None:
    if not sys.platform.startswith("linux"):
        return
    uid = os.getuid()
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{uid}"
    os.environ["XDG_RUNTIME_DIR"] = runtime
    os.environ.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path={runtime}/bus")
    if not os.path.isdir(runtime):
        subprocess.run(["sudo", "-n", "mkdir", "-p", runtime], check=False)
        subprocess.run(["sudo", "-n", "chown", f"{uid}:{uid}", runtime], check=False)
    bus = Path(runtime) / "bus"
    if not bus.exists():
        subprocess.run(
            ["sudo", "-n", "systemctl", "start", f"user@{uid}.service"],
            check=False,
        )
        time.sleep(1)
    probe = subprocess.run(
        ["systemctl", "--user", "status"],
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
    )
    if probe.returncode != 0 and "running" not in (probe.stdout + probe.stderr).lower():
        sys.stderr.write(probe.stdout + probe.stderr)
        raise Fail(
            "systemd --user is not available; cannot vet Linux restart. "
            "The smoke test refuses to skip this: a unit that cannot restart is how a fleet stays bricked."
        )


class WinRegSnapshot:
    def __init__(self):
        self.values: dict[str, Optional[str]] = {}

    def capture(self) -> None:
        if sys.platform not in ("win32", "cygwin"):
            return
        for name in WIN_RUN_VALUES:
            self.values[name] = _reg_query(name)

    def restore(self) -> None:
        if sys.platform not in ("win32", "cygwin"):
            return
        for name in WIN_RUN_VALUES:
            previous = self.values.get(name)
            current = _reg_query(name)
            if previous == current:
                continue
            if previous is None:
                subprocess.run(
                    ["reg", "delete", WIN_RUN_KEY, "/v", name, "/f"],
                    check=False,
                    capture_output=True,
                )
            else:
                subprocess.run(
                    ["reg", "add", WIN_RUN_KEY, "/v", name, "/t", "REG_SZ", "/d", previous, "/f"],
                    check=False,
                    capture_output=True,
                )


def _reg_query(name: str) -> Optional[str]:
    result = subprocess.run(
        ["reg", "query", WIN_RUN_KEY, "/v", name],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        if name.lower() in line.lower() and "REG_SZ" in line:
            return line.split("REG_SZ", 1)[1].strip()
    return None


def maybe_drop_inno_uninstall_key(install_dir: Path) -> None:
    if sys.platform not in ("win32", "cygwin"):
        return
    key = rf"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{WIN_APP_ID}_is1"
    result = subprocess.run(
        ["reg", "query", key, "/v", "InstallLocation"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return
    loc = ""
    for line in result.stdout.splitlines():
        if "REG_SZ" in line:
            loc = line.split("REG_SZ", 1)[1].strip().rstrip("\\")
    needle = str(install_dir).rstrip("\\")
    if loc.lower().startswith(needle.lower()):
        subprocess.run(["reg", "delete", key, "/f"], check=False, capture_output=True)


def isolate_env(home: Path, localappdata: Optional[Path], bin_path: Path, http: str, ws: str) -> dict:
    env = os.environ.copy()
    env["SCALATTICE_HOME"] = str(home)
    env["SCALATTICE_UPDATE_SMOKE"] = "1"
    env["SCALATTICE_UPDATE_BASE"] = http
    env["SCALATTICE_WS_URL"] = ws
    env["SCALATTICE_AGENT_TOKEN"] = SMOKE_TOKEN
    env["SCALATTICE_AGENT_BIN"] = str(bin_path)
    env["SCALATTICE_VERBOSE"] = "1"
    # Keep the real HOME on Unix so systemd/launchd session lookup still works.
    # Windows must isolate USERPROFILE/LOCALAPPDATA — the self-hosted runner
    # has a real agent install.
    if sys.platform in ("win32", "cygwin"):
        env["USERPROFILE"] = str(home)
        env["HOME"] = str(home)
        if localappdata is not None:
            env["LOCALAPPDATA"] = str(localappdata)
            roaming = home / "AppData" / "Roaming"
            roaming.mkdir(parents=True, exist_ok=True)
            env["APPDATA"] = str(roaming)
    path_sep = ";" if sys.platform in ("win32", "cygwin") else ":"
    env["PATH"] = str(bin_path.parent) + path_sep + env.get("PATH", "")
    env["SCALATTICE_INSTALL_DIR"] = str(bin_path.parent)
    if sys.platform.startswith("linux"):
        lib_dest = home / ".local" / "lib" / "scalattice"
        if lib_dest.is_dir():
            existing = env.get("LD_LIBRARY_PATH", "")
            env["LD_LIBRARY_PATH"] = str(lib_dest) + ((":" + existing) if existing else "")
    if localappdata is not None:
        lib = localappdata / "Scalattice" / "lib"
        env["SCALATTICE_LIB_DIR"] = str(lib)
        if lib.is_dir():
            env["PATH"] = str(lib) + path_sep + env["PATH"]
    return env


def wait_registered(state: MockState, timeout: float, before: int) -> None:
    wait_until(
        "agent registered with mock Cloud WS",
        timeout,
        lambda: state.snapshot()["registers"] > before,
    )


def wait_service(bin_path: Path, env: dict, timeout: float) -> None:
    wait_until("background agent running", timeout, lambda: status_running(bin_path, env))


def wait_download(state: MockState, timeout: float, before: int) -> None:
    wait_until(
        "release asset downloaded from mock",
        timeout,
        lambda: state.snapshot()["downloads"] > before,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    parser.add_argument("--timeout", type=float, default=600.0)
    args = parser.parse_args()
    dist = args.dist.resolve()
    asset, seed_zip = detect_assets(dist)
    print(f"==> asset {asset} ({asset.stat().st_size} bytes)")

    tmp = Path(tempfile.mkdtemp(prefix="scalattice-update-smoke-"))
    home = tmp / "home"
    home.mkdir()
    localappdata = tmp / "localappdata" if sys.platform in ("win32", "cygwin") else None
    if localappdata is not None:
        localappdata.mkdir()
    staging = tmp / "staging"
    staging.mkdir()

    if seed_zip is not None:
        extract_zip(seed_zip, staging)
        bin_path = seed_windows(staging, localappdata)  # type: ignore[arg-type]
        update_asset = asset
        update_name = asset.name
    else:
        extract_unix(asset, staging)
        bin_path = seed_unix(staging, home)
        update_asset = asset
        update_name = asset.name

    digest = sha256_file(update_asset)
    # Version from the seeded binary (this release), not from Cargo.toml on disk.
    probe_env = os.environ.copy()
    probe_env["SCALATTICE_UPDATE_SMOKE"] = "1"
    if sys.platform.startswith("linux"):
        lib_dest = home / ".local" / "lib" / "scalattice"
        existing = probe_env.get("LD_LIBRARY_PATH", "")
        probe_env["LD_LIBRARY_PATH"] = str(lib_dest) + ((":" + existing) if existing else "")
    real_version = agent_version(bin_path, probe_env)
    print(f"==> real version {real_version}; advertising {FAKE_VERSION} until first download")

    state = MockState(update_asset, update_name, real_version, digest)
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), make_http_handler(state))
    http_port = httpd.server_address[1]
    http_thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    http_thread.start()
    ws_thread, ws_port = start_ws_server(state)
    http_base = f"http://127.0.0.1:{http_port}"
    ws_url = f"ws://127.0.0.1:{ws_port}/v1/operators/agent/ws"
    print(f"==> mock release {http_base}  mock ws {ws_url}")

    env = isolate_env(home, localappdata, bin_path, http_base, ws_url)
    write_agent_env(
        home,
        {
            "SCALATTICE_AGENT_TOKEN": SMOKE_TOKEN,
            "SCALATTICE_UPDATE_BASE": http_base,
            "SCALATTICE_WS_URL": ws_url,
            "SCALATTICE_UPDATE_SMOKE": "1",
            "SCALATTICE_HOME": str(home),
            "SCALATTICE_AGENT_BIN": str(bin_path),
            "SCALATTICE_INSTALL_DIR": str(bin_path.parent),
            "SCALATTICE_VERBOSE": "1",
        },
    )
    if sys.platform.startswith("linux"):
        lib_dest = home / ".local" / "lib" / "scalattice"
        ld = env.get("LD_LIBRARY_PATH", str(lib_dest))
        env_path = home / ".config" / "scalattice" / "agent.env"
        env_path.write_text(
            env_path.read_text(encoding="utf-8") + f"LD_LIBRARY_PATH={ld}\n",
            encoding="utf-8",
        )
    if localappdata is not None:
        env_path = home / ".config" / "scalattice" / "agent.env"
        env_path.write_text(
            env_path.read_text(encoding="utf-8")
            + f"SCALATTICE_LIB_DIR={localappdata / 'Scalattice' / 'lib'}\n",
            encoding="utf-8",
        )

    win_reg = WinRegSnapshot()
    win_reg.capture()
    ensure_linux_user_systemd()

    failed: Optional[BaseException] = None
    try:
        registers_before = state.snapshot()["registers"]
        result = run_agent(
            bin_path,
            ["set-token", "--token", SMOKE_TOKEN],
            env,
            timeout=120,
        )
        print_cmd(result)
        if result.returncode != 0:
            raise Fail(f"set-token failed ({result.returncode})")
        wait_service(bin_path, env, min(args.timeout, 240))
        wait_registered(state, args.timeout, registers_before)

        print("==> live/remote update (control/update on the running job)")
        downloads_before = state.snapshot()["downloads"]
        registers_live = state.snapshot()["registers"]
        sent = state.send_control("update")
        if sent < 1:
            raise Fail("no live WebSocket client to send remote update")
        wait_download(state, args.timeout, downloads_before)
        wait_until(
            "agent re-registered after live update",
            args.timeout,
            lambda: state.snapshot()["registers"] > registers_live,
        )
        wait_service(bin_path, env, min(args.timeout, 240))
        after = agent_version(bin_path, env)
        if after != real_version:
            raise Fail(f"binary version after live update is {after}, expected {real_version}")
        print("==> live/remote update came back on the new binary")

        print("==> CLI update (scalattice-agent update)")
        state.reset_advertised()
        time.sleep(0.5)
        downloads_before = state.snapshot()["downloads"]
        registers_cli = state.snapshot()["registers"]
        result = run_agent(bin_path, ["update"], env, timeout=int(args.timeout))
        print_cmd(result)
        # Windows CLI exits 0 immediately after spawning Inno; Unix waits for replace+restart.
        if sys.platform not in ("win32", "cygwin") and result.returncode != 0:
            raise Fail(f"CLI update failed ({result.returncode})")
        wait_download(state, args.timeout, downloads_before)
        wait_until(
            "agent re-registered after CLI update",
            args.timeout,
            lambda: state.snapshot()["registers"] > registers_cli,
        )
        wait_service(bin_path, env, min(args.timeout, 240))
        after = agent_version(bin_path, env)
        if after != real_version:
            raise Fail(f"binary version after CLI update is {after}, expected {real_version}")
        print("==> CLI update came back on the new binary")
        print(f"==> mock stats {state.snapshot()}")
        print("==> update smoke passed")
        return 0
    except BaseException as err:  # noqa: BLE001
        failed = err
        print(f"==> update smoke FAILED: {err}", file=sys.stderr)
        dump_logs(home, localappdata)
        snap = state.snapshot()
        print(f"==> mock stats {snap}", file=sys.stderr)
        return 1
    finally:
        try:
            run_agent(bin_path, ["uninstall", "--yes"], env, timeout=90)
        except Exception as err:  # noqa: BLE001
            print(f"==> uninstall during cleanup: {err}", file=sys.stderr)
        if localappdata is not None:
            maybe_drop_inno_uninstall_key(localappdata / "Scalattice" / "bin")
        win_reg.restore()
        try:
            httpd.shutdown()
        except Exception:
            pass
        stop = getattr(ws_thread, "stop_event", None)
        if stop is not None:
            stop.set()
        shutil.rmtree(tmp, ignore_errors=True)
        if failed is not None and not isinstance(failed, Fail):
            raise failed


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Fail as err:
        print(f"==> update smoke FAILED: {err}", file=sys.stderr)
        sys.exit(1)
