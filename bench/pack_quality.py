"""Pack quality, measured on both axes: bytes on the wire, and how deep a chain a client must resolve.

A pack that is smaller because every blob deltas against the one before it costs the client a walk of
that chain for every object it touches. Git caps the depth for that reason, and this reports the depth
distribution alongside the size so the trade is visible rather than argued about.
"""
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(_PROJECT, "target", "release", "gesta")
SRC = os.environ.get("GESTA_SRC", _PROJECT)


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


work = tempfile.mkdtemp(prefix="pack-quality-")
try:
    d = os.path.join(work, "h")
    os.makedirs(d)
    sh(f"{BIN} init", cwd=d)
    rc, out = sh(f"{BIN} git import {SRC}", cwd=d)
    if rc != 0:
        sys.exit(f"import failed: {out[-300:]}")

    sk = socket.socket()
    sk.bind(("127.0.0.1", 0))
    port = str(sk.getsockname()[1])
    sk.close()

    srv = subprocess.Popen(f"{BIN} serve --port {port}", shell=True, cwd=d,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.1)
        clone = os.path.join(work, "c")
        rc, out = sh(f"git clone --quiet http://127.0.0.1:{port}/git/repo {clone}")
        if rc != 0:
            sys.exit(f"clone failed: {out[-300:]}")
    finally:
        srv.terminate()
        srv.wait(timeout=10)

    packdir = os.path.join(clone, ".git", "objects", "pack")
    size = sum(os.path.getsize(os.path.join(packdir, f))
               for f in os.listdir(packdir) if f.endswith(".pack"))

    idx = [os.path.join(packdir, f) for f in os.listdir(packdir) if f.endswith(".idx")][0]
    _, vp = sh(f"git verify-pack -v {idx}")
    if "ok" not in vp.splitlines()[-1]:
        print(f"  not ok: {vp.splitlines()[-1]}")
    chains = [int(m.group(1)) for m in re.finditer(r"chain length = (\d+): \d+ objects", vp)]
    deltas = sum(chains) if chains else 0
    deepest = max(chains) if chains else 0

    print(f"  pack:            {size / 1024:8.1f} KiB")
    print(f"  delta objects:   {deltas:8d}")
    print(f"  deepest chain:   {deepest:8d}")
    if chains:
        print(f"  chains above 10: {sum(c for c in chains if c > 10):8d} objects")

    # git's own pack of the same history.
    sh(f"git clone --quiet --no-local {SRC} {os.path.join(work, 'g')}")
    g = os.path.join(work, "g")
    sh("git repack -adf -q", cwd=g)
    gpack = os.path.join(g, ".git", "objects", "pack")
    gsize = sum(os.path.getsize(os.path.join(gpack, f))
                for f in os.listdir(gpack) if f.endswith(".pack"))
    gidx = [os.path.join(gpack, f) for f in os.listdir(gpack) if f.endswith(".idx")][0]
    _, gvp = sh(f"git verify-pack -v {gidx}")
    gchains = [int(m.group(1)) for m in re.finditer(r"chain length = (\d+): \d+ objects", gvp)]
    print()
    print(f"  git's pack:      {gsize / 1024:8.1f} KiB   ratio {size / gsize:.2f}x")
    print(f"  git deepest:     {(max(gchains) if gchains else 0):8d}")
finally:
    shutil.rmtree(work, ignore_errors=True)
