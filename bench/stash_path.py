#!/usr/bin/env python3
"""Can part of the working tree be set aside without touching the rest?

`git stash --patch` exists because the two changes in a working tree are often unrelated: one is
finished, one is an experiment, and only one of them belongs in the stash. Asking a person to pick hunks
from a prompt is the wrong shape here, so the form is `--path`: the caller names the files it wants set
aside, which is what an agent actually knows.

The failure this guards against is severe and silent. `stash save` resets the whole tree to the branch
head, so a naive path filter would stash one file and then throw away every other edit in the tree. The
work is gone, the stash does not have it, and nothing reported an error.

  a named path is stashed                       it lands in the stash
  the named path goes back to head              the tree shows the committed content
  an unnamed path keeps its edits               this is the check that matters
  an unnamed new file stays on disk             not deleted along with the stashed ones
  one stash holds only the named file           the count is not the whole tree
  popping restores the named path               and leaves the other edit in place
  the whole-tree form still works               the old behaviour did not change
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


def write(root, rel, content):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True) if os.path.dirname(p) else None
    with open(p, "w") as fh:
        fh.write(content)


def read(root, rel):
    p = os.path.join(root, rel)
    if not os.path.isfile(p):
        return None
    with open(p) as fh:
        return fh.read()


def seed(root):
    os.makedirs(root, exist_ok=True)
    sh(["init"], root)
    write(root, "keep.txt", "base\n")
    write(root, "take.txt", "base\n")
    r = sh(["changeset", "create", "seed", "--author", "t", "--json"], root)
    m = re.search(r"(cs[0-9a-f]+)", r.stdout)
    if not m:
        raise SystemExit(f"no changeset: {r.stdout}{r.stderr}")
    sh(["changeset", "commit", m.group(1)], root)


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="stashpath-")
    try:
        root = os.path.join(d, "r")
        seed(root)

        # Two edits, plus a new file that was never committed.
        write(root, "keep.txt", "EDITED keep\n")
        write(root, "take.txt", "EDITED take\n")
        write(root, "brand-new.txt", "never committed\n")

        r = sh(["stash", "save", "--message", "only take", "--path", "take.txt"], root)
        out = r.stdout + r.stderr

        checks.append(("a named path is stashed", r.returncode == 0 and "1 file" in out, out[-200:]))
        checks.append(("the named path goes back to the committed content",
                       read(root, "take.txt") == "base\n", repr(read(root, "take.txt"))))
        checks.append(("an unnamed path keeps its edits",
                       read(root, "keep.txt") == "EDITED keep\n", repr(read(root, "keep.txt"))))
        checks.append(("an unnamed new file stays on disk",
                       read(root, "brand-new.txt") == "never committed\n",
                       repr(read(root, "brand-new.txt"))))

        listing = sh(["stash", "list"], root).stdout
        checks.append(("the stash holds only the named file",
                       "1 file" in listing and "only take" in listing, listing[:220]))

        # Popping brings the stashed content back and leaves the other edit where it was.
        r = sh(["stash", "pop"], root)
        checks.append(("popping restores the named path",
                       r.returncode == 0 and read(root, "take.txt") == "EDITED take\n",
                       repr(read(root, "take.txt"))))
        checks.append(("popping leaves the other edit in place",
                       read(root, "keep.txt") == "EDITED keep\n", repr(read(root, "keep.txt"))))

        # The whole-tree form, unchanged.
        root2 = os.path.join(d, "r2")
        seed(root2)
        write(root2, "keep.txt", "EDITED keep\n")
        write(root2, "take.txt", "EDITED take\n")
        r = sh(["stash", "save", "--message", "everything"], root2)
        out = r.stdout + r.stderr
        checks.append(("the whole-tree form still stashes everything",
                       r.returncode == 0 and "2 file" in out
                       and read(root2, "keep.txt") == "base\n"
                       and read(root2, "take.txt") == "base\n",
                       out[-200:]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} stash-path checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
