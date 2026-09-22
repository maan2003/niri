#!/usr/bin/env python3
"""Real headless compositor/protocol test. Requires working surfaceless EGL."""
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="rho-desktop-test-") as tmp:
    root = Path(tmp)
    config = root / "config.kdl"
    config.write_text('animations { off; }\nhotkey-overlay { skip-at-startup; }\n'
                      'xwayland-satellite { off; }\nlayout { background-color "#315b97"; }\n')
    env = dict(os.environ, XDG_RUNTIME_DIR=tmp)
    with (root / "log").open("w") as log:
        process = subprocess.Popen([binary, "--headless", "--width", "130", "--height", "94",
                                    "--scale", "2", "--config", str(config)], env=env,
                                   stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 15
        while not (root / "rho-desktop/default.json").exists():
            assert process.poll() is None, (root / "log").read_text()
            assert time.monotonic() < deadline, (root / "log").read_text()
            time.sleep(.02)
        path = json.loads((root / "rho-desktop/default.json").read_text())["socket"]
        def connect():
            s = socket.socket(socket.AF_UNIX)
            s.settimeout(5)
            s.connect("\0" + path[1:])
            return s
        def send(s, request):
            s.sendall(json.dumps(request).encode() + b"\n")
        def header(f):
            return json.loads(f.readline())
        for request in [{"type": "hello", "version": 999},
                        {"type": "capture", "output": "headless-1"}]:
            with connect() as s, s.makefile("rb") as f:
                send(s, request)
                assert header(f)["type"] == "error"
                assert f.read() == b""
        with connect() as s, s.makefile("rb") as f:
            send(s, {"type": "hello", "version": 2})
            hello=header(f)
            assert hello.pop("media_socket").startswith("@rho-desktop-")
            assert hello == {"type": "hello", "version": 2, "outputs": [
                {"name": "headless-1", "width": 130, "height": 94, "scale": 2.0}]}
            # Two sequential captures exercise binary/JSON framing without reconnecting.
            for _ in range(2):
                send(s, {"type": "capture", "output": "headless-1"})
                assert header(f) == {"type": "frame", "width": 130, "height": 94}
                pixels = f.read(130 * 94 * 4)
                assert len(pixels) == 130 * 94 * 4
                # Expected independently from the configured background, BGRA channel order.
                assert all(abs(a-b) <= 1 for a,b in zip(pixels[:4], [0x97,0x5b,0x31,255])), pixels[:4]
                assert pixels[:4] == pixels[-4:]
            send(s, {"type": "capture", "output": "missing"})
            assert header(f)["type"] == "error"
        with connect() as s:
            s.sendall(b"x" * 65536)
            try:
                assert s.recv(1) == b""
            except ConnectionResetError:
                pass
        ipc = next(root.glob("niri*.sock"))
        env.update(NIRI_SOCKET=str(ipc), RHO_DESKTOP_SOCKET=str(path))
        outputs = json.loads(subprocess.check_output([binary,"msg","--json","outputs"], env=env))
        assert outputs["headless-1"]["logical"]["width"] == 65
        assert outputs["headless-1"]["logical"]["height"] == 47
        target = root / "capture.png"
        subprocess.run([binary,"capture",str(target)], env=env, check=True)
        png = target.read_bytes()
        assert png[:8] == b"\x89PNG\r\n\x1a\n"
        assert struct.unpack(">II",png[16:24]) == (130,94)
        print("desktop_smoke passed: scaled output, BGRA pixels, framing, rejection, CLI PNG")
    finally:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
