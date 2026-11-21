#!/usr/bin/env python3
"""Does a real git client speak v2 to this server, that is, does it clone and fetch over version 2?

The check is not "the capability list parses". A v2 conversation that the client cannot finish fails
as a stall or as "fatal: invalid server response", and both look like a broken repository. So the test
forces the client to v2 with `-c protocol.version=2` and does the work.
"""
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(cmd, cwd=None, env=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=300, env=env)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_port(port, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def main():
    d = tempfile.mkdtemp(prefix="proto2-")
    work = tempfile.mkdtemp(prefix="proto2-work-")
    try:
        sh(f"{ZF} init", d)
        for i in range(4):
            with open(os.path.join(d, f"f{i}.txt"), "w") as fh:
                fh.write(f"line {i}\n" * 100)
            out = sh(f'{ZF} changeset create "c{i}" --author t --json', d)[1]
            m = re.search(r"(cs[0-9a-f]+)", out)
            if m:
                sh(f"{ZF} changeset commit {m.group(1)}", d)

        port = free_port()
        server = subprocess.Popen(f"{ZF} serve --port {port}", shell=True, cwd=d,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            wait_port(port)
            url = f"http://127.0.0.1:{port}/git/repo"
            env = dict(os.environ, GIT_TRACE_PACKET="1")

            # The capability list, asked for as a v2 client asks.
            rc_adv, adv = sh(f"git -c protocol.version=2 ls-remote {url}", d, env=env)
            # A v2 advertisement starts with `version 2`; v0 starts with a ref or a service line.
            says_v2 = "version 2" in adv or "ls-refs" in adv

            clone = os.path.join(work, "c")
            rc_clone, clone_out = sh(
                f"git -c protocol.version=2 clone -q {url} {clone}", work, env=env
            )
            cloned = rc_clone == 0 and os.path.isdir(os.path.join(clone, ".git"))
            got_files = sorted(os.listdir(clone)) if cloned else []
            has_content = "f0.txt" in got_files

            # A commit on the server, then a v2 fetch.
            with open(os.path.join(d, "new.txt"), "w") as fh:
                fh.write("new\n")
            out = sh(f'{ZF} changeset create "more" --author t --json', d)[1]
            m = re.search(r"(cs[0-9a-f]+)", out)
            if m:
                sh(f"{ZF} changeset commit {m.group(1)}", d)
            rc_fetch, fetch = sh("git -c protocol.version=2 fetch origin", clone, env=env)
            fetched = rc_fetch == 0 and "new" in sh("git show origin/main:new.txt", clone)[1]

            # The conversation used v2 commands, not the v0 want/have exchange.
            used_ls_refs = "ls-refs" in fetch or "ls-refs" in adv
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except Exception:
                server.kill()

        checks = [
            ("the client is told it is speaking v2", says_v2),
            ("a v2 clone works", cloned),
            ("the clone has the file content", has_content),
            ("a v2 fetch works", fetched),
            ("the conversation used ls-refs", used_ls_refs),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        if not cloned:
            print(clone_out[-700:])
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} protocol v2 checks pass")
            return 0
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
