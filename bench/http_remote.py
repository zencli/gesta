#!/usr/bin/env python3
"""Prove the HTTP remote works over a real socket, not an in-memory shortcut.

Starts `gesta serve` on a free port, then clones, pushes and pulls between
two real directories through http://127.0.0.1. Every assertion checks the
observable result (files on disk, branch heads), never just an exit code.

Run: python3 bench/http_remote.py
"""

import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request

# The binary and the project are found relative to this file, not from a fixed mount path. A suite
# that only runs in one container cannot be part of the check a change is measured against.
_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/zf_http"
results = []


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).split("\n")[:5]:
            print(f"         {line}")


def run(*args, cwd=None):
    r = subprocess.run([ZF, *args], capture_output=True, text=True, cwd=cwd)
    return r.returncode, (r.stdout + r.stderr).strip()


def write(d, path, text):
    full = os.path.join(d, path)
    parent = os.path.dirname(full)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(full, "w") as fh:
        fh.write(text)


def read(d, path):
    with open(os.path.join(d, path)) as fh:
        return fh.read()


def newcs(d, msg, author="agent"):
    rc, out = run("changeset", "create", msg, "--author", author, cwd=d)
    for w in out.split():
        if w.startswith("cs"):
            return w
    return None


def commit(d, msg, author="agent"):
    cs = newcs(d, msg, author)
    if cs:
        run("changeset", "commit", cs, cwd=d)
    return cs


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def wait_for_health(port, timeout=20.0):
    deadline = time.time() + timeout
    url = f"http://127.0.0.1:{port}/v1/health"
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as r:
                if r.status == 200 and r.read().strip() == b"ok":
                    return True
        except Exception:
            time.sleep(0.15)
    return False


def main():
    if not os.path.exists(ZF):
        sys.exit(f"binary not found: {ZF}")
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    # --- origin repository with content ---
    origin = os.path.join(ROOT, "origin")
    os.makedirs(origin)
    run("init", cwd=origin)
    write(origin, "src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")
    write(origin, "README.md", "# project\n")
    write(origin, ".gitignore", ".env\n")
    write(origin, ".env", "SECRET=do-not-push\n")
    commit(origin, "initial commit")

    port = free_port()
    server = subprocess.Popen(
        [ZF, "serve", "--bind", "127.0.0.1", "--port", str(port)],
        cwd=origin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        up = wait_for_health(port)
        record("server starts and answers /v1/health", up, f"port={port}")
        if not up:
            out, err = server.communicate(timeout=5)
            record("server startup output", False, f"{out}\n{err}")
            return finish()

        # --- health and branch listing ---
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/v1/branches", timeout=5) as r:
            listing = r.read().decode()
        record("GET /v1/branches lists the branch and its head",
               "main\tcs" in listing, listing)

        # --- clone over http ---
        clone_dir = os.path.join(ROOT, "clone1")
        rc, out = run("remote", "clone", f"http://127.0.0.1:{port}", clone_dir, cwd=ROOT)
        ok = rc == 0 and os.path.exists(os.path.join(clone_dir, "src/main.rs"))
        content = read(clone_dir, "src/main.rs") if ok else ""
        record("clone over http materialises the files",
               ok and "fn main()" in content, f"{out} | {content!r}")

        # --- .gitignore survived the round trip ---
        env_absent = not os.path.exists(os.path.join(clone_dir, ".env"))
        record("clone does not carry ignored files", env_absent,
               f".env present={not env_absent}")

        # --- push over http ---
        write(clone_dir, "src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n")
        commit(clone_dir, "add lib.rs from clone")
        rc, out = run("remote", "push", "origin", cwd=clone_dir)
        ok = rc == 0 and "pushed" in out and "over http" in out
        record("push over http succeeds", ok, out)

        # origin (the served repo) must now see the new file
        st_rc, st = run("status", cwd=origin)
        write_ok = os.path.exists(os.path.join(origin, "src/lib.rs"))
        record("pushed changeset landed in the served repository",
               ok and write_ok, f"file on disk={write_ok}\n{st[:200]}")

        # --- the server refuses a push that would rewind a branch ---
        stale = os.path.join(ROOT, "stale")
        run("remote", "clone", f"http://127.0.0.1:{port}", stale, cwd=ROOT)
        # Move the remote forward first.
        write(clone_dir, "src/forward.rs", "// forward\n")
        commit(clone_dir, "advance remote")
        run("remote", "push", "origin", cwd=clone_dir)
        # Now make the stale clone diverge and try to push its old head.
        write(stale, "src/stale.rs", "// stale branch work\n")
        commit(stale, "stale work")
        rc, out = run("remote", "push", "origin", cwd=stale)
        rejected = rc != 0 and ("rejected" in out.lower() or "pull first" in out.lower())
        record("push that would rewind the remote is refused", rejected, out)

        # --- pull over http brings the new commit into the stale clone ---
        rc, out = run("remote", "pull", "origin", cwd=stale)
        pulled = rc == 0 and os.path.exists(os.path.join(stale, "src/forward.rs"))
        record("pull over http fetches and merges remote work", pulled, out)

        # ... and the two derived tables a bundle does not carry are rebuilt on arrival. Blame is
        # this store's own index over the history, so without the rebuild every arrived file answers
        # "never committed" while `fsck` reports the branch tree as stale.
        rc, out = run("blame", "src/forward.rs", cwd=stale)
        record(
            "blame answers for a file that arrived by an http pull",
            rc == 0 and "never committed" not in out.lower() and "cs" in out.lower(),
            out,
        )
        rc, out = run("fsck", cwd=stale)
        record(
            "the branch tree a pull brings in is not stale",
            rc == 0 and "stale" not in out.lower(),
            out,
        )

        # --- clone through a real socket in a second process, verifying content ---
        clone2 = os.path.join(ROOT, "clone2")
        rc, out = run("remote", "clone", f"http://127.0.0.1:{port}", clone2, cwd=ROOT)
        ok = rc == 0 and read(clone2, "src/lib.rs").startswith("pub fn add")
        record("a second clone sees the pushed history", ok, out)

        # --- unknown branch is a clean 404, not a crash ---
        try:
            urllib.request.urlopen(
                f"http://127.0.0.1:{port}/v1/bundle?branch=nope", timeout=5
            )
            status = 200
        except urllib.error.HTTPError as e:
            status = e.code
        record("unknown branch returns 404", status == 404, f"status={status}")

        # --- https is a real transport now. A public host without a gesta
        # server must produce a clean failure, not a hang or a TLS error.
        rc, out = run("remote", "clone", "https://example.com/repo",
                      os.path.join(ROOT, "x"), cwd=ROOT)
        record("https reaches a public host and reports a clean failure",
               rc != 0 and ("404" in out or "no branches" in out.lower() or "cannot list" in out.lower()),
               out[:250])

        # --- a bare word is a filesystem path, so it must fail as a missing
        # local repository rather than being mistaken for a host name.
        rc, out = run("remote", "clone", "example.com", os.path.join(ROOT, "y"), cwd=ROOT)
        record("a bare path is treated as local, not as a host",
               rc != 0 and "no repository at" in out, out[:200])

        # --- a scheme that is neither http nor https is refused ---
        rc, out = run("remote", "clone", "ftp://example.com/repo",
                      os.path.join(ROOT, "z"), cwd=ROOT)
        record("an unsupported scheme is refused with a clear message",
               rc != 0, out[:200])

    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()

    return finish()


def finish():
    passed = sum(1 for _, ok, _ in results if ok)
    total = len(results)
    print(f"\n{passed}/{total} HTTP checks pass")
    if passed < total:
        print("\nfailing:")
        for name, ok, _ in results:
            if not ok:
                print(f"  - {name}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
