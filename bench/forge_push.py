#!/usr/bin/env python3
"""Push to this tool's own git server using only the forge client, then read it back with git.

This is the one step that proves the forge client works end to end rather than in parts: a store is
built, served over HTTP, pushed to with `forge push`, and the result is cloned with a real git client
and its file contents compared. Every piece of that already existed except the client, so a failure
here is a failure in one of the three requests a push is made of.

The git binary is needed for the final clone and only there. If it is absent the step skips rather than
fails, because a machine without git can still push to a forge; it just cannot be checked this way.
"""

import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ZF = os.environ.get("GESTA_BIN", "/work/gesta/target/release/gesta")


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def run(args, cwd, env=None, timeout=120):
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, env=e, timeout=timeout)


def changeset_id(out: str):
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", out)
    return m.group(1) if m else None


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1
    if shutil.which("git") is None:
        print("git is not installed, so the push cannot be read back: skipped")
        return 0

    base = tempfile.mkdtemp(prefix="forge-push-")
    src = os.path.join(base, "src")
    remote = os.path.join(base, "remote")
    clone = os.path.join(base, "clone")
    for d in (src, remote, clone):
        os.makedirs(d, exist_ok=True)

    server = None
    try:
        # A store with one committed file.
        run([ZF, "init"], src)
        with open(os.path.join(src, "a.txt"), "w") as fh:
            fh.write("alpha\n")
        r = run([ZF, "changeset", "create", "seed", "--json"], src)
        cid = changeset_id(r.stdout)
        if not cid:
            print(f"could not create a changeset: {r.stdout}{r.stderr}")
            return 1
        run([ZF, "changeset", "commit", cid], src)

        # An empty repository to receive the push, served over HTTP.
        run([ZF, "init"], remote)
        port = free_port()
        server = subprocess.Popen(
            [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port)],
            cwd=remote,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for _ in range(60):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("the server never came up")
            return 1

        url = f"http://127.0.0.1:{port}/git/remote"
        p = run(
            [ZF, "forge", "push", url, "main", "--remote-ref", "refs/heads/main"],
            src,
            env={"GESTA_FORGE_TOKEN": "dummy"},
        )
        if p.returncode != 0:
            print(f"the push failed: {p.stdout}{p.stderr}")
            return 1
        if "pushed" not in p.stdout:
            print(f"the push printed no confirmation: {p.stdout}{p.stderr}")
            return 1

        # Read it back with a real git client, which is the only way to know the objects arrived
        # rather than just the ref.
        g = run(["git", "clone", "-q", url, clone], base)
        if g.returncode != 0:
            print(f"git could not clone what the forge client pushed: {g.stderr}")
            return 1
        with open(os.path.join(clone, "a.txt")) as fh:
            body = fh.read()
        if body.strip() != "alpha":
            print(f"the clone has the wrong contents: {body!r}")
            return 1
        log = run(["git", "log", "--oneline"], clone)
        if "seed" not in log.stdout:
            print(f"the clone has no history: {log.stdout}{log.stderr}")
            return 1

        print("a push with the forge client is readable by git")
        print("  the ref moved, the objects arrived, and the file contents match")
        return 0
    finally:
        if server is not None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
