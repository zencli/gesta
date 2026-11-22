"""How much bigger is a pack of independently compressed objects than git's own?

The server sends every object compressed on its own, so a file that changed once between two commits
is sent twice in full. Git sends the second one as a delta against the first. This measures the gap on
a real repository rather than estimating it.

Run: python3 bench/pack_size.py [--repo PATH]
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile


def sh(cmd, cwd=None):
    """(returncode, output). Every call in this file reads exactly these two things."""
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--repo",
        default=os.environ.get(
            "GESTA_REPO",
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        ),
    )
    args = ap.parse_args()

    src = args.repo
    if not os.path.isdir(os.path.join(src, ".gesta")):
        sys.exit(f"not a gesta repository: {src}")

    work = tempfile.mkdtemp(prefix="pack-size-")
    try:
        # Import the repository into a fresh one, so this measures a history this project did not
        # write through the server and can be repeated on any repository.
        d = os.path.join(work, "h")
        os.makedirs(d)
        binp = os.path.join(src, "target", "release", "gesta")
        sh(f"{binp} init", cwd=d)
        rc, out = sh(f"{binp} git import {src}", cwd=d)
        if rc != 0:
            print(f"  import failed: {out[-300:]}")
            return 1
        _, log = sh(f"{binp} log --oneline", cwd=d)
        commits = len(log.strip().splitlines())
        print(f"  imported {commits} commit(s)")

        # What a git clone of the served repository costs, which is the payload a client downloads.
        import socket as _s

        _sk = _s.socket()
        _sk.bind(("127.0.0.1", 0))
        port = str(_sk.getsockname()[1])
        _sk.close()

        _, listing = sh(f"{binp} ls", cwd=d)
        obj = listing.split()
        print(f"  {len(obj)} object(s) in the store")

        # The server's pack, measured by asking it for everything through a real git client.
        srv = subprocess.Popen(f"{binp} serve --port {port}", shell=True, cwd=d,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            import socket
            import time
            for _ in range(50):
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                        break
                except OSError:
                    time.sleep(0.1)
            clone = os.path.join(work, "c")
            rc, out = sh(f"git clone --quiet http://127.0.0.1:{port}/git/repo {clone}")
            if rc != 0:
                print(f"  clone failed: {out[-300:]}")
                return 1
            # git records the pack it received.
            packdir = os.path.join(clone, ".git", "objects", "pack")
            ours = sum(os.path.getsize(os.path.join(packdir, f))
                       for f in os.listdir(packdir) if f.endswith(".pack"))
            print(f"  this server's pack:   {ours / 1024:8.1f} KiB")
        finally:
            srv.terminate()
            srv.wait(timeout=10)

        # Git's own pack of the same history, repacked tight.
        sh(f"git clone --quiet --no-local {src} {os.path.join(work, 'g')}")
        g = os.path.join(work, "g")
        sh("git repack -adf -q", cwd=g)
        gpack = os.path.join(g, ".git", "objects", "pack")
        theirs = sum(os.path.getsize(os.path.join(gpack, f))
                     for f in os.listdir(gpack) if f.endswith(".pack"))
        print(f"  git's pack:           {theirs / 1024:8.1f} KiB")
        print()
        if theirs:
            print(f"  ratio: {ours / theirs:.2f}x git's pack")
        return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
