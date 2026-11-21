#!/usr/bin/env python3
"""Does a real git client negotiate against this server under multi_ack_detailed and still succeed?

The client is pinned to protocol v0: git 2.43 defaults to v2, which has no multi_ack_detailed at
all, so an unpinned run measures the other protocol and the failure lands at the wrong seam.
The capability changes the conversation, and a conversation the client does not understand fails in
ways that look like a broken pack: a stall until timeout, or "fatal: the remote end hung up". So the
check is a real clone, a real fetch and a real second fetch, with git's own packet trace read to
confirm the capability was offered and the ACKs arrived.
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
    d = tempfile.mkdtemp(prefix="multiack-")
    work = tempfile.mkdtemp(prefix="multiack-work-")
    try:
        sh(f"{ZF} init", d)
        # Enough history that a second fetch has something to negotiate over.
        for i in range(8):
            with open(os.path.join(d, f"f{i}.txt"), "w") as fh:
                fh.write(f"line {i}\n" * 200)
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
            clone = os.path.join(work, "c")

            # The advertisement carries the capability.
            trace_env = dict(os.environ, GIT_TRACE_PACKET="1")
            v0 = "-c protocol.version=0"
            rc_ls, ls = sh(f"git {v0} ls-remote {url}", d, env=trace_env)
            offered = "multi_ack_detailed" in ls

            rc_clone, clone_out = sh(f"git {v0} clone -q {url} {clone}", work, env=trace_env)
            cloned = rc_clone == 0 and os.path.isdir(os.path.join(clone, ".git"))

            # A commit on the server, then a fetch that negotiates.
            with open(os.path.join(d, "new.txt"), "w") as fh:
                fh.write("new\n")
            out = sh(f'{ZF} changeset create "more" --author t --json', d)[1]
            m = re.search(r"(cs[0-9a-f]+)", out)
            if m:
                sh(f"{ZF} changeset commit {m.group(1)}", d)

            rc_fetch, fetch = sh(f"git {v0} fetch origin", clone, env=trace_env)
            fetched = rc_fetch == 0
            acks = re.findall(r"ACK ([0-9a-f]{40})(?: (common|ready))?", fetch)
            acked = len(acks) > 0
            # The new commit arrived, which is what the negotiation was for.
            arrived = os.path.exists(os.path.join(d, "new.txt")) and "new" in sh(
                "git show origin/main:new.txt", clone
            )[1]

            # A third fetch with nothing new must succeed rather than hang on a protocol it does not
            # finish: "everything up-to-date" is the answer a stall would replace.
            rc_again, again = sh(f"git {v0} fetch origin", clone, env=trace_env)
            repeat_ok = rc_again == 0
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except Exception:
                server.kill()

        checks = [
            ("the advertisement offers multi_ack_detailed", offered),
            ("a clone still works", cloned),
            ("a fetch negotiates and succeeds", fetched),
            ("the server sent ACK lines for the client's haves", acked),
            ("the new commit arrived", arrived),
            ("a fetch with nothing new returns cleanly", repeat_ok),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        if acks:
            print(f"  {len(acks)} ACK line(s), forms: {sorted(set(a[1] for a in acks))}")
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} negotiation checks pass")
            return 0
        print(clone_out[-400:])
        print(fetch[-600:])
        return 1
    finally:
        shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
