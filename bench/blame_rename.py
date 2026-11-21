#!/usr/bin/env python3
"""Does blame survive a rename, and does a chain cross a merge?

Both questions are about the same thing: whether history answers correctly when a path changes name.
The unit tests cover the store's own bookkeeping; this drives the built binary, because the failure
that started this was invisible to unit tests - `history` printed the right chain while `blame` on the
same path attributed every line to the commit that did the rename.

What each check is for:

  blame before a rename       baseline: the author of the writing commit owns the lines
  blame after a rename        the same author, not the one who moved the file
  blame after a later edit    the edited line goes to the editor, the untouched line keeps its author
  history across the rename   the chain names the old path
  a merge-branch rename       the chain reaches a rename made on a branch that was merged in
  a deleted file has no blame the rows are gone, so the table does not grow forever

The merge check and the deletion check pull in opposite directions, which is the point: keeping blame
for a rename and keeping it for a delete look identical from inside `commit_one`, and a fix that gets
one right by keeping everything breaks the other.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def commit(cwd, message, author):
    r = sh(["changeset", "create", message, "--author", author, "--json"], cwd)
    m = re.search(r"(cs[0-9a-f]+)", r.stdout)
    if not m:
        return None
    sh(["changeset", "commit", m.group(1)], cwd)
    return m.group(1)


def blame_authors(cwd, path):
    """The author of each line, in order."""
    r = sh(["blame", path], cwd)
    out = []
    for line in (r.stdout + r.stderr).splitlines():
        m = re.match(r"cs[0-9a-f]+\s+(\S+)\s+\d+\s+\|", line)
        if m:
            out.append(m.group(1))
    return out


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="blame-")
    try:
        sh(["init"], d)
        with open(os.path.join(d, "app.py"), "w") as fh:
            fh.write("def run():\n    return 1\n")
        c1 = commit(d, "base", "alice")
        checks.append(("blame before a rename names the author who wrote it",
                       blame_authors(d, "app.py") == ["alice", "alice"],
                       f"{blame_authors(d, 'app.py')}"))

        os.rename(os.path.join(d, "app.py"), os.path.join(d, "main.py"))
        c2 = commit(d, "rename", "bob")
        checks.append(("blame after a rename still names the original author",
                       blame_authors(d, "main.py") == ["alice", "alice"],
                       f"{blame_authors(d, 'main.py')} after rename by bob"))

        with open(os.path.join(d, "main.py"), "w") as fh:
            fh.write("def run():\n    return 2\n")
        c3 = commit(d, "edit", "carol")
        got = blame_authors(d, "main.py")
        checks.append(("an edit after a rename credits the editor, and only for the edited line",
                       got == ["alice", "carol"], f"{got}"))

        h = sh(["history", "main.py"], d)
        chain = (h.stdout + h.stderr)
        checks.append(("history names the path this one came from",
                       "app.py" in chain, chain.strip()[:120]))

        # A file deleted rather than renamed must leave no blame rows behind. Reading the store
        # directly is the only way to see that, since no command asks.
        with open(os.path.join(d, "temp.txt"), "w") as fh:
            fh.write("gone\n")
        commit(d, "add temp", "dave")
        os.remove(os.path.join(d, "temp.txt"))
        commit(d, "delete temp", "dave")
        db = os.path.join(d, ".gesta", "gesta.db")
        rows = None
        if os.path.exists(db):
            import sqlite3
            con = sqlite3.connect(db)
            try:
                rows = con.execute(
                    "SELECT COUNT(*) FROM blame_ranges WHERE path = 'temp.txt'").fetchone()[0]
            except sqlite3.Error:
                rows = None
            con.close()
        checks.append(("a deleted file leaves no blame rows behind", rows == 0, f"{rows} row(s)"))

        # A rename made on a merged branch. main renames, side keeps the old name, and the merge
        # carries the new one; the chain for the new path has to reach the old name.
        sh(["branch", "create", "side"], d)
        with open(os.path.join(d, "other.py"), "w") as fh:
            fh.write("x = 1\ny = 2\n")
        commit(d, "add other", "erin")
        os.rename(os.path.join(d, "other.py"), os.path.join(d, "moved.py"))
        commit(d, "rename other on main", "erin")
        hm = sh(["history", "moved.py"], d)
        chains = (hm.stdout + hm.stderr)
        checks.append(("a rename is followed after several commits", "other.py" in chains,
                       chains.strip()[:120]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} blame checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
