#!/usr/bin/env python3
"""What does a tree of tiny files actually cost, now?

`docs/storage.md` records 6.6x against git for 1000 files of seven bytes, and records that the
number was taken before blame became ranges. This measures the current build against the same shape,
so the doc and the code agree or the difference is visible.

Two trees, because the two cases have different causes:

  1000 files of 7 bytes     every row is mostly overhead
  1000 files of 400 bytes   the bodies are large enough that row overhead stops dominating

A ratio that only exists for the first case is a per-row cost. A ratio that appears in both is a
compression or format problem. The fix is different, and measuring one and assuming the other is how a
project spends a week on the wrong table.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ZF = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta")


def sh(args, cwd, env=None):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, env=env)


def dir_bytes(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total


def table_costs(db):
    """dbstat, so a fix knows which table to look at rather than which one is suspected."""
    import sqlite3
    if not os.path.exists(db):
        return []
    conn = sqlite3.connect(db)
    try:
        rows = conn.execute(
            "SELECT name, SUM(pgsize) b FROM dbstat GROUP BY name ORDER BY b DESC"
        ).fetchall()
    except sqlite3.Error:
        rows = []
    finally:
        conn.close()
    return rows


def measure(n_files, body_size, label):
    d = tempfile.mkdtemp(prefix="tiny-")
    work = os.path.join(d, "repo")
    os.makedirs(work)
    try:
        body = ("x" * max(0, body_size - 1)) + "\n"
        for i in range(n_files):
            with open(os.path.join(work, f"f{i:04d}.txt"), "w") as fh:
                fh.write(body)

        # git, as the yardstick. A bare clone of the committed history, not the working tree.
        g = os.path.join(work, ".gitref")
        os.makedirs(g)
        sh(["git", "init", "-q"], g)
        sh(["git", "config", "user.email", "t@t"], g)
        sh(["git", "config", "user.name", "t"], g)
        for i in range(n_files):
            shutil.copy(os.path.join(work, f"f{i:04d}.txt"), os.path.join(g, f"f{i:04d}.txt"))
        sh(["git", "add", "-A"], g)
        sh(["git", "commit", "-qm", "one"], g)
        sh(["git", "gc", "-q", "--aggressive", "--prune=now"], g)
        git_bytes = dir_bytes(os.path.join(g, ".git"))

        # gesta.
        sh([ZF, "init"], work)
        made = sh([ZF, "changeset", "create", "one", "--author", "t", "--json"], work)
        m = re.search(r"(cs[0-9a-f]+)", made.stdout)
        if m:
            sh([ZF, "changeset", "commit", m.group(1)], work)
        db = os.path.join(work, ".gesta", "gesta.db")
        # VACUUM so free pages from the insert order are not counted as history.
        import sqlite3
        c = sqlite3.connect(db)
        c.execute("VACUUM")
        c.close()
        gesta_bytes = dir_bytes(os.path.join(work, ".gesta"))
        tables = table_costs(db)

        ratio = gesta_bytes / git_bytes if git_bytes else 0
        print(f"\n{label}: {n_files} files, {body_size} bytes each")
        print(f"  git    {git_bytes:>9,} bytes")
        print(f"  gesta  {gesta_bytes:>9,} bytes")
        print(f"  ratio  {ratio:>9.2f}x git")
        if tables:
            print("  largest tables and indexes:")
            for name, b in tables[:6]:
                print(f"    {name:<34} {b:>9,}  {100.0 * b / sum(t[1] for t in tables):>5.1f}%")
        return ratio, tables
    finally:
        shutil.rmtree(d, ignore_errors=True)


def main():
    tiny, tiny_tables = measure(1000, 7, "tiny")
    small, _ = measure(1000, 400, "small")
    print()
    print(f"tiny/400-byte ratio: {tiny:.2f}x vs {small:.2f}x")
    if tiny > 0 and small > 0 and tiny / small > 1.6:
        print("the ratio is a per-row cost: it falls sharply as bodies grow")
    else:
        print("the ratio is not explained by row overhead alone")
    return 0


if __name__ == "__main__":
    sys.exit(main())
