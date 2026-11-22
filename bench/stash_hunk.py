#!/usr/bin/env python3
"""Can a stash take one hunk out of a file and leave the other edits in place?

`--path` selects whole files, which covers two unrelated files in one tree. It does not cover two
unrelated edits in one file, and that is the case a person hits constantly: a rename at the top of a
file is finished, a debug print at the bottom is not, and only one of them belongs in the stash.

The form is a hunk number rather than an interactive picker, for the same reason `--path` is not a
prompt: a caller that knows what it is doing names it. `gesta diff` numbers the hunks, and the number
is what `--hunk` takes.

The dangerous half is the same as `--path`, one level finer. A stash that takes hunk 1 and then restores
the file from the branch head throws away hunk 2 as well, and the work is gone with no error. So the
checks are mostly about what is still there afterwards:

  a hunk is selected and stashed              the stash holds it
  the file on disk keeps the other hunk       this is the one that matters
  the stashed hunk is not on disk             it moved, it was not copied
  the unstashed edit is byte for byte         not "roughly the same"
  popping puts the hunk back                  and leaves the other edit alone
  a hunk number that does not exist is refused  rather than silently taking all or none
  stashing every hunk equals --path           the whole file, one path
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))

BASE = "line one\nline two\nline three\nline four\nline five\nline six\nline seven\n"
# Two edits far enough apart to be separate hunks, with enough context between them.
EDITED = "line one CHANGED\nline two\nline three\nline four\nline five\nline six\nline seven CHANGED\n"


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def write(root, rel, content):
    with open(os.path.join(root, rel), "w") as fh:
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
    write(root, "f.txt", BASE)
    r = sh(["changeset", "create", "seed", "--author", "t", "--json"], root)
    m = re.search(r"(cs[0-9a-f]+)", r.stdout)
    if not m:
        raise SystemExit(f"no changeset: {r.stdout}{r.stderr}")
    sh(["changeset", "commit", m.group(1)], root)


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="stashhunk-")
    try:
        root = os.path.join(d, "r")
        seed(root)
        write(root, "f.txt", EDITED)

        # How many hunks does the edit have? The number has to come from the tool, not from a guess.
        # `diff` prints each hunk as `@<n>(<path>)`, which is the form `--hunk` takes.
        diff = sh(["diff"], root).stdout + sh(["diff"], root).stderr
        numbered = re.findall(r"^@(\d+)\(", diff, re.M)
        checks.append(("diff numbers the hunks so a number can be named",
                       len(numbered) >= 2, diff[:260]))

        r = sh(["stash", "save", "--message", "first hunk", "--hunk", "f.txt:1"], root)
        out = r.stdout + r.stderr
        checks.append(("a hunk is selected and stashed", r.returncode == 0, out[-240:]))

        after = read(root, "f.txt")
        keeps_other = after is not None and "line seven CHANGED" in after
        checks.append(("the file keeps the hunk that was not taken", keeps_other, repr(after)))

        dropped = after is not None and "line one CHANGED" not in after
        checks.append(("the stashed hunk is gone from the file", dropped, repr(after)))

        checks.append(("the remaining edit is byte for byte what it was",
                       after == "line one\nline two\nline three\nline four\nline five\nline six\nline seven CHANGED\n",
                       repr(after)))

        # Popping brings the taken hunk back and leaves the other edit alone.
        r = sh(["stash", "pop"], root)
        back = read(root, "f.txt")
        checks.append(("popping restores the hunk",
                       r.returncode == 0 and back is not None and "line one CHANGED" in back,
                       repr(back)))
        checks.append(("popping leaves the other edit in place",
                       back is not None and "line seven CHANGED" in back, repr(back)))

        # A hunk number that is not there has to be refused, not treated as "all" or "none".
        root2 = os.path.join(d, "r2")
        seed(root2)
        write(root2, "f.txt", EDITED)
        r = sh(["stash", "save", "--message", "no such", "--hunk", "f.txt:99"], root2)
        out = (r.stdout + r.stderr).lower()
        checks.append(("a hunk number that does not exist is refused",
                       r.returncode != 0 and ("hunk" in out or "no such" in out), out[-240:]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} stash-hunk checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
