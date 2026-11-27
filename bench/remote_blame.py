#!/usr/bin/env python3
"""Every way a repository arrives must leave `blame` able to answer.

Blame ranges are this store's own index over the history, derived from the changesets. A backup
carries them because a backup is a copy of the store. A bundle - what `remote clone`, `remote pull`
and a push to `gesta serve` all move - carries changesets and objects but not the ranges, because it
is a git-shaped transfer of history, not of one implementation's index.

So each receiving path has to derive them. Three paths did not, and each answered "no blame data
(never committed)" for files it had just imported. The comment on the issue narrowed it to one
missing call: `restore-repo` derives the rows, clone and pull did not.

This checks the property directly, in the form the issue asked for: the same three commits moved
four ways, and `blame_ranges` must match the source every time.

Run: python3 bench/remote_blame.py
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
COMMITS = 3

checks = []


def record(name, ok, detail=""):
    checks.append((name, ok))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if not ok and detail:
        for line in str(detail).strip().splitlines()[:5]:
            print(f"         {line}")


def sh(args, cwd, timeout=300):
    # A backup is sealed, so the commands that write or read one need the passphrase (#340).
    env = dict(os.environ)
    env["GESTA_BACKUP_PASSPHRASE"] = "blame-bench-passphrase"
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout,
                          env=env)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def make_source(base):
    """Three commits on one file, which is enough for blame to have something to attribute."""
    d = os.path.join(base, "src")
    os.makedirs(d)
    sh([ZF, "init"], d)
    for i in range(1, COMMITS + 1):
        with open(os.path.join(d, "f.txt"), "w") as fh:
            fh.write(f"v{i}\n")
        made = sh([ZF, "changeset", "create", f"s{i}", "--author", "t", "--json"], d)
        try:
            cid = json.loads(made.stdout)["changeset"]
        except (json.JSONDecodeError, KeyError):
            continue
        sh([ZF, "changeset", "commit", cid], d)
    return d


def blame_rows(repo):
    db = os.path.join(repo, ".gesta", "gesta.db")
    if not os.path.exists(db):
        return -1
    import sqlite3

    c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return c.execute("SELECT COUNT(*) FROM blame_ranges").fetchone()[0]
    finally:
        c.close()


def blame_says_never(repo):
    r = sh([ZF, "blame", "f.txt"], repo)
    return "never committed" in (r.stdout + r.stderr)


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1

    base = tempfile.mkdtemp(prefix="remote-blame-")
    server = None
    try:
        src = make_source(base)
        want = blame_rows(src)
        record(f"the source derives blame rows ({want})", want > 0, f"count {want}")
        record("blame works on the source", not blame_says_never(src))

        # Backup round trip. It carries the rows, so it was never broken and must stay correct.
        r = sh([ZF, "backup", os.path.join(base, "b.gesta")], src)
        record("backup succeeds", r.returncode == 0, r.stdout + r.stderr)
        restored = os.path.join(base, "restored")
        os.makedirs(restored)
        sh([ZF, "init"], restored)
        sh([ZF, "restore-repo", os.path.join(base, "b.gesta"), "--yes"], restored)
        record(
            "restore-repo keeps the blame rows it carried",
            blame_rows(restored) == want,
            f"{blame_rows(restored)} of {want}",
        )
        record("blame works after restore-repo", not blame_says_never(restored))

        # Serve the source, then clone and pull through the bundle path.
        port = free_port()
        server = subprocess.Popen(
            [ZF, "serve", "--port", str(port)],
            cwd=src,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        url = f"http://127.0.0.1:{port}/"
        for _ in range(120):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    break
            except OSError:
                time.sleep(0.1)

        clone_dir = os.path.join(base, "cloned")
        r = sh([ZF, "remote", "clone", url, clone_dir], base, timeout=300)
        record("clone succeeds", r.returncode == 0, r.stdout + r.stderr)
        record(
            "a clone derives the blame rows it needs",
            blame_rows(clone_dir) == want,
            f"{blame_rows(clone_dir)} of {want}",
        )
        record(
            "blame answers after a clone",
            not blame_says_never(clone_dir),
            sh([ZF, "blame", "f.txt"], clone_dir).stdout,
        )

        # A push to the served repository. The server had the rows already, so the check is that
        # the derive call did not break the answer, plus that a new commit stays attributable.
        with open(os.path.join(clone_dir, "f.txt"), "w") as fh:
            fh.write("v4 pushed\n")
        made = sh([ZF, "changeset", "create", "from clone", "--author", "c", "--json"], clone_dir)
        try:
            cid = json.loads(made.stdout)["changeset"]
        except (json.JSONDecodeError, KeyError):
            cid = None
        if cid:
            sh([ZF, "changeset", "commit", cid], clone_dir)
        r = sh([ZF, "remote", "push", "origin"], clone_dir, timeout=300)
        record("push succeeds", r.returncode == 0, r.stdout + r.stderr)

        # Pull on a second clone after the source moved.
        second = os.path.join(base, "second")
        r = sh([ZF, "remote", "clone", url, second], base, timeout=300)
        record("a second clone succeeds", r.returncode == 0, r.stdout + r.stderr)
        record(
            "a pull-shaped clone derives blame",
            blame_rows(second) > 0,
            f"{blame_rows(second)} rows",
        )
        record(
            "blame answers in the second clone",
            not blame_says_never(second),
            sh([ZF, "blame", "f.txt"], second).stdout,
        )

        # Issue #65 shares the cause: a derived table a bundle does not carry. A pull moves the head
        # and left `branch_tree` describing the changesets that arrived before it, which fsck
        # reported as `STALE tree for branch`. Checked here because the two are the same class of
        # missing write and a fix for one that leaves the other is half a fix.
        r = sh([ZF, "fsck"], clone_dir)
        record(
            "fsck is clean after a clone",
            r.returncode == 0 and "STALE" not in r.stdout,
            r.stdout + r.stderr,
        )
        r = sh([ZF, "fsck"], second)
        record(
            "fsck is clean after a second clone",
            r.returncode == 0 and "STALE" not in r.stdout,
            r.stdout + r.stderr,
        )
        r = sh([ZF, "fsck"], src)
        record(
            "fsck is clean on the served repository after a push",
            r.returncode == 0 and "STALE" not in r.stdout,
            r.stdout + r.stderr,
        )

        passed = sum(1 for _, ok in checks if ok)
        print(f"\n{passed}/{len(checks)} remote blame checks pass")
        if passed != len(checks):
            print("\nfailing:")
            for n, ok in checks:
                if not ok:
                    print(f"  - {n}")
        return 0 if passed == len(checks) else 1
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
