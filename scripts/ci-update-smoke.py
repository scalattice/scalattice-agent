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
import signal
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
# GUI-subsystem agent + piped stdio can hang on Windows; hide the window.
CREATE_NO_WINDOW = 0x08000000


class Fail(Exception):
    pass


def log(msg: str) -> None:
    print(msg, flush=True)


def start_deadline_watchdog(seconds: float) -> None:
    def boom() -> None:
        time.sleep(seconds)
        print(
            f"==> update smoke FAILED: wall-clock deadline ({int(seconds)}s)",
            file=sys.stderr,
            flush=True,
        )
        os._exit(1)

    threading.Thread(target=boom, name="smoke-deadline", daemon=True).start()


def describe_returncode(code: int) -> str:
    if code < 0:
        try:
            return f"{code} ({signal.Signals(-code).name})"
        except ValueError:
            return f"{code} (signal {-code})"
    nt = {
        0xC0000005: "STATUS_ACCESS_VIOLATION",
        0xC0000135: "STATUS_DLL_NOT_FOUND",
        0xC0000139: "STATUS_ENTRYPOINT_NOT_FOUND",
        0xC0000409: "STATUS_STACK_BUFFER_OVERRUN",
    }
    if code in nt:
        return f"{code} (0x{code:08X} {nt[code]})"
    return str(code)


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
                log(f"==> ok: {desc}")
                return
        except Exception as err:  # noqa: BLE001
            last = err
        time.sleep(0.4)
    extra = f" ({last})" if last else ""
    raise Fail(f"timeout waiting for {desc}{extra}")


def kill_process_tree(pid: int) -> None:
    if sys.platform in ("win32", "cygwin"):
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            capture_output=True,
            check=False,
            timeout=30,
        )
        return
    try:
        os.kill(pid, 9)
    except OSError:
        pass


def run_agent(bin_path: Path, args: list[str], env: dict, timeout: int = 180, quiet: bool = False) -> subprocess.CompletedProcess:
    cmdline = [str(bin_path), *args]
    if not quiet:
        log(f"==> {' '.join(cmdline)}")
    kwargs: dict = {
        "args": cmdline,
        "env": env,
        "text": True,
        "encoding": "utf-8",
        "errors": "replace",
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "stdin": subprocess.DEVNULL,
    }
    if sys.platform in ("win32", "cygwin"):
        kwargs["creationflags"] = CREATE_NO_WINDOW
    proc = subprocess.Popen(**kwargs)
    deadline = time.time() + timeout
    while True:
        remaining = deadline - time.time()
        if remaining <= 0:
            break
        try:
            stdout, stderr = proc.communicate(timeout=min(remaining, 0.5))
            return subprocess.CompletedProcess(
                cmdline, proc.returncode, stdout or "", stderr or ""
            )
        except subprocess.TimeoutExpired:
            # Windows: the CLI can exit while a detached `foreground` child still
            # holds inherited stdout/stderr, so communicate() never sees EOF.
            if proc.poll() is not None:
                log("==> CLI exited; background agent still holds inherited stdio")
                return subprocess.CompletedProcess(
                    cmdline, proc.returncode, "", ""
                )
    kill_process_tree(proc.pid)
    if quiet:
        return subprocess.CompletedProcess(cmdline, -1, "", f"timed out after {timeout}s")
    raise Fail(f"{' '.join(args)} timed out after {timeout}s")


def print_cmd(result: subprocess.CompletedProcess) -> None:
    if result.stdout:
        sys.stdout.write(result.stdout)
        if not result.stdout.endswith("\n"):
            sys.stdout.write("\n")
        sys.stdout.flush()
    if result.stderr:
        sys.stderr.write(result.stderr)
        if not result.stderr.endswith("\n"):
            sys.stderr.write("\n")
        sys.stderr.flush()


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
        # Seed from the tarball; the update asset must be the DMG (agent installs
        # that now). Prefer a CI-built DMG when present, else pack one later.
        asset = dist / "scalattice-agent-aarch64-apple-darwin.tar.gz"
        if not asset.is_file():
            raise Fail(f"missing {asset}")
        return asset, None
    if system in ("win32", "cygwin"):
        setup = find_dist_file(
            dist,
            "ScalatticeAgentSetup-x86_64.exe",
        )
        zipped = find_dist_file(
            dist,
            "scalattice-agent-x86_64-pc-windows-msvc.zip",
        )
        return setup, zipped
    raise Fail(f"unsupported platform {system}/{machine}")


def package_macos_smoke_dmg(agent_bin: Path, dest_dmg: Path) -> Path:
    """Build a minimal Scalattice Agent.app DMG for the update smoke.

    Production releases ship a notarized DMG; the agent installs that whole
    bundle. The smoke must advertise/serve the same asset name or the live
    update never downloads (checksum miss → downloads stay at 0).
    """
    if dest_dmg.is_file():
        return dest_dmg
    stage = dest_dmg.parent / "dmg-stage"
    if stage.exists():
        shutil.rmtree(stage)
    app = stage / "Scalattice Agent.app"
    macos_dir = app / "Contents" / "MacOS"
    macos_dir.mkdir(parents=True)
    target = macos_dir / "scalattice-agent"
    shutil.copy2(agent_bin, target)
    target.chmod(0o755)
    (app / "Contents" / "Info.plist").write_text(
        """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>scalattice-agent</string>
  <key>CFBundleIdentifier</key>
  <string>cloud.scalattice.agent.smoke</string>
  <key>CFBundleName</key>
  <string>Scalattice Agent</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
</dict>
</plist>
""",
        encoding="utf-8",
    )
    dest_dmg.parent.mkdir(parents=True, exist_ok=True)
    last_detail = ""
    for attempt in range(1, 7):
        if dest_dmg.exists():
            dest_dmg.unlink()
        # Drop leftover Scalattice attaches — GHA runners sometimes leave
        # diskimages-helper wedged ("Resource busy").
        info = subprocess.run(
            ["hdiutil", "info"],
            capture_output=True,
            text=True,
            check=False,
        )
        current_dev = ""
        busy_devs: set[str] = set()
        for line in (info.stdout or "").splitlines():
            if line.startswith("/dev/"):
                current_dev = line.split()[0]
            elif "calattice" in line.lower() and current_dev:
                busy_devs.add(current_dev)
                current_dev = ""
        for dev in sorted(busy_devs):
            subprocess.run(
                ["hdiutil", "detach", "-force", dev],
                capture_output=True,
                check=False,
            )
        result = subprocess.run(
            [
                "hdiutil",
                "create",
                "-volname",
                "Scalattice Agent",
                "-srcfolder",
                str(stage),
                "-ov",
                "-format",
                "UDZO",
                str(dest_dmg),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0 and dest_dmg.is_file():
            break
        last_detail = (result.stderr or result.stdout or "").strip() or str(
            result.returncode
        )
        log(f"==> hdiutil create busy/failed (attempt {attempt}/6); retrying…")
        time.sleep(attempt * 2)
    else:
        raise Fail(f"hdiutil create failed for smoke DMG: {last_detail}")
    shutil.rmtree(stage, ignore_errors=True)
    log(f"==> packed smoke DMG {dest_dmg.name} ({dest_dmg.stat().st_size} bytes)")
    return dest_dmg


def find_dist_file(dist: Path, name: str) -> Path:
    roots = [dist, dist.parent, Path.cwd(), Path.cwd() / "dist"]
    seen: set[Path] = set()
    for root in roots:
        try:
            candidate = (root / name).resolve()
        except OSError:
            continue
        if candidate in seen:
            continue
        seen.add(candidate)
        if candidate.is_file():
            return candidate
    raise Fail(f"missing {name} (looked in {dist} and {Path.cwd()})")


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


def _u16(data: bytes, off: int) -> int:
    return int.from_bytes(data[off : off + 2], "little")


def _u32(data: bytes, off: int) -> int:
    return int.from_bytes(data[off : off + 4], "little")


def _pe_rva_to_off(sections: list[tuple[int, int, int, int]], rva: int) -> Optional[int]:
    for vaddr, vsize, rawptr, rawsize in sections:
        span = max(vsize, rawsize)
        if vaddr <= rva < vaddr + span:
            return rawptr + (rva - vaddr)
    return None


def _pe_cstring(data: bytes, off: int, limit: int = 512) -> str:
    end = data.find(b"\x00", off, min(len(data), off + limit))
    if end < 0:
        return ""
    return data[off:end].decode("ascii", "ignore")


def pe_imported_dlls(path: Path) -> dict[str, list[str]]:
    """PE import table: dll-name lower -> imported symbol names (or #ordinal)."""
    try:
        data = path.read_bytes()
    except OSError:
        return {}
    if len(data) < 0x40 or data[:2] != b"MZ":
        return {}
    e_lfanew = _u32(data, 0x3C)
    if e_lfanew + 24 > len(data) or data[e_lfanew : e_lfanew + 4] != b"PE\x00\x00":
        return {}
    coff = e_lfanew + 4
    nsec = _u16(data, coff + 2)
    opt_size = _u16(data, coff + 16)
    opt = coff + 20
    magic = _u16(data, opt)
    pe32plus = magic == 0x20B
    if magic not in (0x10B, 0x20B):
        return {}
    num_rva_off = 108 if pe32plus else 92
    dd = opt + (112 if pe32plus else 96)
    num_rva = _u32(data, opt + num_rva_off)
    if num_rva < 2 or dd + 16 > len(data):
        return {}
    import_rva = _u32(data, dd + 8)
    if import_rva == 0:
        return {}
    sec0 = opt + opt_size
    sections: list[tuple[int, int, int, int]] = []
    for i in range(nsec):
        o = sec0 + i * 40
        if o + 40 > len(data):
            break
        sections.append((_u32(data, o + 12), _u32(data, o + 8), _u32(data, o + 20), _u32(data, o + 16)))
    thunk_size = 8 if pe32plus else 4
    ordinal_bit = 1 << (63 if pe32plus else 31)

    def rva_off(rva: int) -> Optional[int]:
        return _pe_rva_to_off(sections, rva)

    out: dict[str, list[str]] = {}
    desc = rva_off(import_rva)
    if desc is None:
        return {}
    while desc + 20 <= len(data):
        orig = _u32(data, desc)
        name_rva = _u32(data, desc + 12)
        first = _u32(data, desc + 16)
        if orig == 0 and name_rva == 0 and first == 0:
            break
        name_off = rva_off(name_rva) if name_rva else None
        dll = _pe_cstring(data, name_off).lower() if name_off is not None else ""
        names: list[str] = []
        thunk_rva = orig or first
        thunk_off = rva_off(thunk_rva) if thunk_rva else None
        if thunk_off is not None:
            cur = thunk_off
            while cur + thunk_size <= len(data):
                raw = int.from_bytes(data[cur : cur + thunk_size], "little")
                if raw == 0:
                    break
                if raw & ordinal_bit:
                    names.append(f"#{raw & 0xFFFF}")
                else:
                    ibn = rva_off(raw)
                    if ibn is not None and ibn + 2 < len(data):
                        names.append(_pe_cstring(data, ibn + 2))
                cur += thunk_size
        if dll:
            out.setdefault(dll, []).extend(n for n in names if n)
        desc += 20
    return out


def windows_system32() -> Path:
    root = os.environ.get("SystemRoot") or os.environ.get("WINDIR") or r"C:\Windows"
    return Path(root) / "System32"


def windows_has_nvcuda() -> bool:
    sys32 = windows_system32()
    wow = sys32.parent / "SysWOW64"
    return (sys32 / "nvcuda.dll").is_file() or (wow / "nvcuda.dll").is_file()


def windows_dll_present(name: str, search: list[Path]) -> bool:
    key = name.lower()
    # API-set names are resolved by the OS; they are not files on disk.
    if key.startswith("api-ms-win-") or key.startswith("ext-ms-win-"):
        return True
    for directory in search:
        if not directory.is_dir():
            continue
        if (directory / name).is_file() or (directory / key).is_file():
            return True
        try:
            if any(p.name.lower() == key for p in directory.iterdir() if p.is_file()):
                return True
        except OSError:
            continue
    return False


def windows_unresolved_imports(bindir: Path, libdir: Path) -> list[str]:
    search = [bindir, libdir, windows_system32()]
    missing: list[str] = []
    seen: set[str] = set()
    pes = [bindir / "scalattice-agent.exe"]
    for folder in (bindir, libdir):
        if folder.is_dir():
            pes.extend(p for p in folder.glob("*.dll") if p.is_file())
    for pe in pes:
        if not pe.is_file():
            continue
        for dll in pe_imported_dlls(pe):
            if dll in seen:
                continue
            seen.add(dll)
            if not windows_dll_present(dll, search):
                missing.append(dll)
    return missing


def windows_nvcuda_exports(bindir: Path, libdir: Path) -> list[str]:
    names: list[str] = []
    seen: set[str] = set()
    pes = [bindir / "scalattice-agent.exe"]
    for folder in (bindir, libdir):
        if folder.is_dir():
            pes.extend(p for p in folder.glob("*.dll") if p.is_file())
    for pe in pes:
        if not pe.is_file():
            continue
        for symbol in pe_imported_dlls(pe).get("nvcuda.dll", []):
            if symbol and symbol not in seen:
                seen.add(symbol)
                names.append(symbol)
    fallback = (
        "cuInit",
        "cuDriverGetVersion",
        "cuDeviceGet",
        "cuDeviceGetCount",
        "cuDeviceGetName",
        "cuDeviceGetAttribute",
        "cuDeviceGetUuid",
        "cuDeviceGetUuid_v2",
        "cuDeviceTotalMem_v2",
        "cuCtxCreate_v2",
        "cuCtxDestroy_v2",
        "cuCtxPushCurrent_v2",
        "cuCtxPopCurrent_v2",
        "cuCtxSetCurrent",
        "cuCtxGetCurrent",
        "cuCtxGetDevice",
        "cuCtxSynchronize",
        "cuMemAlloc_v2",
        "cuMemFree_v2",
        "cuMemGetInfo_v2",
        "cuMemcpyHtoD_v2",
        "cuMemcpyDtoH_v2",
        "cuMemcpyDtoD_v2",
        "cuMemcpyHtoDAsync_v2",
        "cuMemcpyDtoHAsync_v2",
        "cuLaunchKernel",
        "cuModuleLoadData",
        "cuModuleLoadDataEx",
        "cuModuleGetFunction",
        "cuModuleUnload",
        "cuGetErrorString",
        "cuGetErrorName",
        "cuStreamCreate",
        "cuStreamDestroy_v2",
        "cuStreamSynchronize",
        "cuEventCreate",
        "cuEventDestroy_v2",
        "cuEventRecord",
        "cuEventSynchronize",
    )
    for symbol in fallback:
        if symbol not in seen:
            names.append(symbol)
    return names


def windows_vcvars64() -> Optional[Path]:
    vswhere = Path(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe")
    if not vswhere.is_file():
        return None
    kwargs: dict = {
        "capture_output": True,
        "text": True,
        "timeout": 30,
        "check": False,
    }
    if sys.platform in ("win32", "cygwin"):
        kwargs["creationflags"] = CREATE_NO_WINDOW
    result = subprocess.run(
        [
            str(vswhere),
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ],
        **kwargs,
    )
    line = (result.stdout or "").strip().splitlines()
    if not line:
        return None
    bat = Path(line[0].strip()) / "VC" / "Auxiliary" / "Build" / "vcvars64.bat"
    return bat if bat.is_file() else None


def _c_ident(name: str, idx: int) -> str:
    if name.isidentifier() and not name.startswith("#"):
        return name
    return f"scalattice_nvcuda_stub_{idx}"


def compile_windows_nvcuda_stub(bindir: Path, libdir: Path) -> Path:
    vcvars = windows_vcvars64()
    if vcvars is None:
        raise Fail(
            "hosted Windows has no nvcuda.dll (NVIDIA driver) and vswhere/vcvars64.bat "
            "was not found to compile a CI load stub"
        )
    exports = windows_nvcuda_exports(bindir, libdir)
    work = bindir / ".nvcuda_stub"
    work.mkdir(parents=True, exist_ok=True)
    src = work / "nvcuda_stub.c"
    defn = work / "nvcuda.def"
    c_lines = [
        "/* CI-only loader stub; not a real NVIDIA driver. */",
        "int scalattice_nvcuda_ci_stub(void) { return 999; }",
    ]
    def_lines = ["LIBRARY nvcuda", "EXPORTS"]
    for idx, name in enumerate(exports):
        ident = _c_ident(name, idx)
        c_lines.append(f"int {ident}(void) {{ return 999; }}")
        if name.startswith("#"):
            try:
                ordinal = int(name[1:])
            except ValueError:
                continue
            def_lines.append(f"    {ident} @{ordinal} NONAME")
        elif ident == name:
            def_lines.append(f"    {name}")
        else:
            def_lines.append(f"    {name}={ident}")
    src.write_text("\n".join(c_lines) + "\n", encoding="utf-8")
    defn.write_text("\n".join(def_lines) + "\n", encoding="utf-8")
    # Python's list2cmdline escapes inner quotes, so `cmd /c call "C:\Program
    # Files\...\vcvars64.bat"` becomes a command name that cmd cannot find.
    # Drive vcvars + cl from a .bat in this folder instead.
    built = work / "nvcuda.dll"
    script = work / "build.bat"
    script.write_text(
        "\r\n".join(
            [
                "@echo off",
                "setlocal",
                f'call "{vcvars}"',
                "if errorlevel 1 exit /b 1",
                "cl /nologo /LD /O1 /Fe:nvcuda.dll nvcuda_stub.c /link /DLL /DEF:nvcuda.def /OUT:nvcuda.dll",
                "exit /b %ERRORLEVEL%",
                "",
            ]
        ),
        encoding="utf-8",
    )
    log(f"==> compiling nvcuda stub via {vcvars}")
    kwargs: dict = {
        "capture_output": True,
        "text": True,
        "timeout": 180,
        "check": False,
        "cwd": str(work),
    }
    if sys.platform in ("win32", "cygwin"):
        kwargs["creationflags"] = CREATE_NO_WINDOW
    compiled = subprocess.run(["cmd.exe", "/c", str(script)], **kwargs)
    if compiled.returncode != 0 or not built.is_file():
        detail = (compiled.stderr or compiled.stdout or "").strip()
        raise Fail(f"failed to compile nvcuda.dll stub: {detail or compiled.returncode}")
    dest = bindir / "nvcuda.dll"
    shutil.copy2(built, dest)
    shutil.copy2(built, libdir / "nvcuda.dll")
    shutil.rmtree(work, ignore_errors=True)
    return dest


def install_windows_nvcuda_stub(bindir: Path, libdir: Path) -> None:
    if windows_has_nvcuda():
        log("==> system nvcuda.dll present")
        return
    if (bindir / "nvcuda.dll").is_file() or (libdir / "nvcuda.dll").is_file():
        return
    compiled = compile_windows_nvcuda_stub(bindir, libdir)
    log(f"==> compiled {compiled.name} stub (hosted Windows has no NVIDIA driver)")
    missing = windows_unresolved_imports(bindir, libdir)
    if missing:
        log(f"==> still unresolved after nvcuda stub: {', '.join(missing)}")


def dump_windows_install_layout(localappdata: Optional[Path]) -> None:
    if localappdata is None:
        return
    for sub in ("bin", "lib"):
        folder = localappdata / "Scalattice" / sub
        print(f"==> {folder}")
        if not folder.is_dir():
            print("    (missing)")
            continue
        for item in sorted(folder.iterdir(), key=lambda p: p.name.lower()):
            if item.is_file():
                print(f"    {item.name} {item.stat().st_size}")
    exe = localappdata / "Scalattice" / "bin" / "scalattice-agent.exe"
    if exe.is_file():
        imports = pe_imported_dlls(exe)
        if imports:
            print("==> scalattice-agent.exe imports " + ", ".join(sorted(imports)))
        missing = windows_unresolved_imports(
            localappdata / "Scalattice" / "bin",
            localappdata / "Scalattice" / "lib",
        )
        if missing:
            print("==> unresolved DLL imports: " + ", ".join(missing))


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
    libdir.mkdir(parents=True, exist_ok=True)
    dest = bindir / "scalattice-agent.exe"
    shutil.copy2(exe, dest)
    dlls = [p for p in staging.rglob("*.dll") if p.is_file()]
    log(f"==> seeding {len(dlls)} DLL(s) next to the exe and into lib/")
    for dll in dlls:
        log(f"    {dll.relative_to(staging)}")
        shutil.copy2(dll, bindir / dll.name)
        shutil.copy2(dll, libdir / dll.name)
    for helper in ("scalattice-run.cmd", "launch-background.vbs", "launch-tray.vbs"):
        src = exe.parent / helper
        if src.is_file():
            shutil.copy2(src, bindir / helper)
    install_windows_nvcuda_stub(bindir, libdir)
    return dest


def agent_version(bin_path: Path, env: dict) -> str:
    if sys.platform in ("win32", "cygwin"):
        pe = windows_pe_product_version(bin_path)
        if pe:
            log(f"==> {bin_path} ProductVersion {pe}")
            return pe.lstrip("v")
    result = run_agent(bin_path, ["--version"], env, timeout=30)
    print_cmd(result)
    if result.returncode != 0:
        raise Fail(f"--version failed ({describe_returncode(result.returncode)})")
    text = (result.stdout or result.stderr or "").strip().split()
    if not text:
        raise Fail("empty --version output")
    return text[-1].lstrip("v")


def windows_pe_product_version(bin_path: Path) -> Optional[str]:
    literal = str(bin_path).replace("'", "''")
    ps = f"(Get-Item -LiteralPath '{literal}').VersionInfo.ProductVersion"
    kwargs: dict = {
        "capture_output": True,
        "text": True,
        "timeout": 30,
        "check": False,
    }
    if sys.platform in ("win32", "cygwin"):
        kwargs["creationflags"] = CREATE_NO_WINDOW
    result = subprocess.run(["powershell", "-NoProfile", "-Command", ps], **kwargs)
    ver = (result.stdout or "").strip()
    if result.returncode != 0 or not ver:
        return None
    return ver.split()[0].strip()


def write_agent_env(home: Path, extra: dict[str, str]) -> Path:
    cfg = home / ".config" / "scalattice"
    cfg.mkdir(parents=True, exist_ok=True)
    path = cfg / "agent.env"
    lines = [f"{k}={v}" for k, v in extra.items()]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def status_running(bin_path: Path, env: dict) -> bool:
    if sys.platform in ("win32", "cygwin"):
        pid_path = bin_path.parent / "background.pid"
        if pid_path.is_file():
            try:
                pid = int(pid_path.read_text(encoding="utf-8", errors="replace").strip())
            except ValueError:
                pid = 0
            if pid and windows_pid_alive(pid):
                return True
    result = run_agent(bin_path, ["status"], env, timeout=30, quiet=True)
    text = f"{result.stdout}\n{result.stderr}"
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("Service") and "running" in stripped and "not " not in stripped:
            return True
        if stripped.startswith("Agent") and stripped.endswith("running"):
            return True
    return False


def windows_pid_alive(pid: int) -> bool:
    kwargs: dict = {
        "capture_output": True,
        "text": True,
        "timeout": 15,
        "check": False,
    }
    if sys.platform in ("win32", "cygwin"):
        kwargs["creationflags"] = CREATE_NO_WINDOW
    result = subprocess.run(
        ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
        **kwargs,
    )
    out = result.stdout or ""
    return str(pid) in out and "scalattice" in out.lower()


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

    if sys.platform.startswith("linux"):
        for cmd in (
            ["systemctl", "--user", "status", "scalattice-agent.service", "--no-pager", "-l", "-n", "30"],
            ["journalctl", "--user", "-u", "scalattice-agent.service", "-n", "40", "--no-pager"],
        ):
            print(f"==> {' '.join(cmd)}")
            try:
                out = subprocess.run(cmd, capture_output=True, text=True, timeout=20, check=False)
                sys.stdout.write(out.stdout or "")
                sys.stderr.write(out.stderr or "")
            except Exception as err:  # noqa: BLE001
                print(f"  ({err})")

    if sys.platform in ("win32", "cygwin"):
        dump_windows_install_layout(localappdata)
        if localappdata is not None:
            exe = localappdata / "Scalattice" / "bin" / "scalattice-agent.exe"
            if exe.is_file():
                print(f"==> {exe} status")
                try:
                    out = subprocess.run(
                        [str(exe), "status"],
                        capture_output=True,
                        text=True,
                        timeout=30,
                        check=False,
                        env={
                            **os.environ,
                            "SCALATTICE_UPDATE_SMOKE": "1",
                            "SCALATTICE_HOME": str(home),
                            "USERPROFILE": str(home),
                            "LOCALAPPDATA": str(localappdata),
                            "SCALATTICE_INSTALL_DIR": str(localappdata / "Scalattice" / "bin"),
                            "PATH": (
                                str(localappdata / "Scalattice" / "bin")
                                + ";"
                                + str(localappdata / "Scalattice" / "lib")
                                + ";"
                                + os.environ.get("PATH", "")
                            ),
                        },
                    )
                    sys.stdout.write(out.stdout or "")
                    sys.stderr.write(out.stderr or "")
                except Exception as err:  # noqa: BLE001
                    print(f"  ({err})")
        print("==> tasklist scalattice-agent.exe")
        try:
            out = subprocess.run(
                ["tasklist", "/FI", "IMAGENAME eq scalattice-agent.exe", "/V"],
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )
            sys.stdout.write(out.stdout or "")
            sys.stderr.write(out.stderr or "")
        except Exception as err:  # noqa: BLE001
            print(f"  ({err})")


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
    # Isolated smoke must not initialize the host GPU (self-hosted Windows runner).
    env.setdefault("CUDA_VISIBLE_DEVICES", "")
    if sys.platform == "darwin":
        # Keep DMG installs out of the real /Applications on the runner.
        app_root = home / "Applications"
        app_root.mkdir(parents=True, exist_ok=True)
        env["SCALATTICE_MACOS_APP_ROOT"] = str(app_root)
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
    try:
        sys.stdout.reconfigure(line_buffering=True)
        sys.stderr.reconfigure(line_buffering=True)
    except (AttributeError, OSError):
        pass
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    parser.add_argument("--timeout", type=float, default=600.0)
    args = parser.parse_args()
    if sys.platform in ("win32", "cygwin"):
        start_deadline_watchdog(min(args.timeout * 3, 12 * 60))
    dist = args.dist.resolve()
    asset, seed_zip = detect_assets(dist)
    log(f"==> asset {asset} ({asset.stat().st_size} bytes)")

    tmp = Path(tempfile.mkdtemp(prefix="scalattice-update-smoke-"))
    home = tmp / "home"
    home.mkdir()
    localappdata = tmp / "localappdata" if sys.platform in ("win32", "cygwin") else None
    if localappdata is not None:
        localappdata.mkdir()
    staging = tmp / "staging"
    staging.mkdir()

    if seed_zip is not None:
        log(f"==> extracting {seed_zip.name}")
        extract_zip(seed_zip, staging)
        log("==> seeding Windows install dir")
        bin_path = seed_windows(staging, localappdata)  # type: ignore[arg-type]
        update_asset = asset
        update_name = asset.name
    else:
        log(f"==> extracting {asset.name}")
        extract_unix(asset, staging)
        bin_path = seed_unix(staging, home)
        if sys.platform == "darwin":
            # Agent now downloads ScalatticeAgentSetup-aarch64.dmg, not the tar.gz.
            dist_dmg = dist / "ScalatticeAgentSetup-aarch64.dmg"
            if dist_dmg.is_file():
                update_asset = dist_dmg
                log(f"==> using CI DMG {update_asset.name}")
            else:
                update_asset = package_macos_smoke_dmg(
                    bin_path, tmp / "ScalatticeAgentSetup-aarch64.dmg"
                )
            update_name = update_asset.name
        else:
            update_asset = asset
            update_name = asset.name

    log(f"==> hashing {update_asset.name}")
    digest = sha256_file(update_asset)
    # Version from the seeded binary (this release), not from Cargo.toml on disk.
    probe_env = os.environ.copy()
    probe_env["SCALATTICE_UPDATE_SMOKE"] = "1"
    probe_env.setdefault("CUDA_VISIBLE_DEVICES", "")
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
            **(
                {"SCALATTICE_MACOS_APP_ROOT": str(home / "Applications")}
                if sys.platform == "darwin"
                else {}
            ),
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
            raise Fail(f"set-token failed ({describe_returncode(result.returncode)})")
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
            raise Fail(f"CLI update failed ({describe_returncode(result.returncode)})")
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
