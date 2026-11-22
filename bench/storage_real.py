#!/usr/bin/env python3
"""Storage against git, on trees whose files are actually different from each other.

`tiny_files.py` writes 1000 copies of the same body. git compresses that to almost nothing, because
every blob is the same blob, so the ratio it prints measures how well each tool deduplicates one
repeated file. That is a real property but not the one a repository has.

These trees have distinct contents at several granularities, because the size of the gap depends
entirely on how much the files share:

  distinct   every file different, nothing to share
  shared     every file has the same 200-line prologue and a different tail
  json       small JSON documents with the same shape and different values

Run: python3 bench/storage_real.py
"""

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=300)


def dir_bytes(path):
    total = 0
    for root, _d, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total


def write_tree(root, kind, n):
    os.makedirs(root, exist_ok=True)
    for i in range(n):
        p = os.path.join(root, f"f{i:05d}.txt")
        if kind == "distinct":
            # Nothing shared: a different word per line, per file.
            body = "\n".join(f"line {j} of file {i} {chr(97 + (i + j) % 26)}" for j in range(20)) + "\n"
        elif kind == "shared":
            prologue = "\n".join(f"// module boilerplate line {j}" for j in range(200))
            body = prologue + f"\n// {i} unique tail value {i * 7919 % 100000}\n"
        else:  # json
            body = json.dumps(
                {"id": i, "name": f"item-{i}", "active": i % 2 == 0, "score": i * 3 % 97,
                 "tags": ["alpha", "beta"], "note": f"record {i}"},
                separators=(",", ":"),
            ) + "\n"
        with open(p, "w") as fh:
            fh.write(body)


def measure(kind, n):
    d = tempfile.mkdtemp(prefix=f"storage-{kind}-")
    work = os.path.join(d, "repo")
    os.makedirs(work)
    try:
        write_tree(work, kind, n)

        g = os.path.join(d, "gitref")
        os.makedirs(g)
        sh(["git", "init", "-q"], g)
        sh(["git", "config", "user.email", "t@t"], g)
        sh(["git", "config", "user.name", "t"], g)
        for name in os.listdir(work):
            shutil.copy(os.path.join(work, name), os.path.join(g, name))
        sh(["git", "add", "-A"], g)
        sh(["git", "commit", "-qm", "one"], g)
        sh(["git", "gc", "-q", "--aggressive", "--prune=now"], g)
        git_bytes = dir_bytes(os.path.join(g, ".git"))

        sh([ZF, "init"], work)
        made = sh([ZF, "changeset", "create", "one", "--author", "t", "--json"], work)
        try:
            cid = json.loads(made.stdout).get("changeset")
        except json.JSONDecodeError:
            cid = None
        if cid:
            sh([ZF, "changeset", "commit", cid], work)
        # Both sides compacted: VACUUM for the database, gc for the pack.
        db = os.path.join(work, ".gesta", "gesta.db")
        c = sqlite3.connect(db)
        c.execute("VACUUM")
        c.close()
        gesta_bytes = dir_bytes(os.path.join(work, ".gesta"))

        return git_bytes, gesta_bytes
    finally:
        shutil.rmtree(d, ignore_errors=True)


def main() -> int:
    if not os.path.exists(ZF):
        print(f"binary not found: {ZF}")
        return 1
    if shutil.which("git") is None:
        print("git is not installed: skipped")
        return 0

    n = 1000
    print(f"{n} files per tree, both sides fully compacted\n")
    print(f"{'tree':<10} {'git':>12} {'gesta':>12} {'ratio':>8}")
    rows = []
    for kind in ("distinct", "shared", "json"):
        g, z = measure(kind, n)
        ratio = z / g if g else 0
        rows.append((kind, g, z, ratio))
        print(f"{kind:<10} {g:>12,} {z:>12,} {ratio:>7.2f}x")

    worst = max(rows, key=lambda r: r[3])
    print(f"\nworst case: {worst[0]} at {worst[3]:.2f}x")
    # The claim this replaces was one number for one synthetic shape. A tree with nothing shared is
    # the one that costs, and it is the one a repository of generated files looks like.
    return 0


if __name__ == "__main__":
    sys.exit(main())
