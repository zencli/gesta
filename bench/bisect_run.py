#!/usr/bin/env python3
"""Does one command drive a whole bisect search, including the case git calls "skip"?

`git bisect run` exists because the manual loop does not scale: a 30 commit range is 5 answers typed by
hand, and a typo in one of them names the wrong commit with no error. The convention that makes it work
is the exit code - 0 good, anything else bad, and 125 means the changeset cannot be tested at all.

That last case is the one worth measuring. A build that fails to compile on an old changeset has to be
dropped from the range without being reported as either answer; if it is counted as bad, the search
names a commit that was never tested.

  a clean range finds the first bad changeset     the answer is the commit that first fails
  the answer is not the last bad one              a search that returns the tip is not searching
  the search asks about fewer than all of them    the point of halving, measured as a count
  a skipped candidate is not the answer           exit 125 drops it, and the real culprit is still found
  the working tree is back at the head afterwards  a search that leaves the tree on an old commit is a trap
  a command that cannot start is an error          not evidence about the changeset
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


def commit(cwd, name, content):
    with open(os.path.join(cwd, "f.txt"), "w") as fh:
        fh.write(content)
    r = sh(["changeset", "create", name, "--author", "a", "--json"], cwd)
    m = re.search(r"cs[0-9a-f]+", r.stdout)
    if not m:
        raise SystemExit(f"could not create a changeset for {name}: {r.stdout}{r.stderr}")
    sh(["changeset", "commit", m.group(0)], cwd)
    return m.group(0)


def head_of(cwd):
    r = sh(["log", "--json"], cwd)
    m = re.search(r"cs[0-9a-f]+", r.stdout)
    return m.group(0) if m else None


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="bisect-run-")
    try:
        root = os.path.join(d, "r")
        os.makedirs(root)
        sh(["init"], root)

        c1 = commit(root, "one", "1\n")
        commit(root, "two", "2\n")
        c3 = commit(root, "three is bad", "3\n")
        commit(root, "four", "4\n")
        tip = commit(root, "five", "5\n")

        # The file says 3, 4 or 5 on the bad side of the range. Exit 1 when it matches, because
        # exit 0 means good to a bisect and a bare `grep -q` is exactly backwards here.
        failing = 'grep -qE "^(3|4|5)$" f.txt && exit 1 || exit 0'

        sh(["bisect", "start", c1, tip], root)
        r = sh(["bisect", "run", failing], root)
        out = r.stdout + r.stderr
        answer = c3

        checks.append(("the search names the first bad changeset", answer in out, out[-220:]))
        checks.append(("the answer is not the tip of the range", tip not in out.split("first bad")[-1],
                       out[-160:]))

        # Count the questions: 4 candidates must take at most 3 asks, never 4.
        asked = len(re.findall(r"^(testing|good|bad)\b", out, re.M))
        checks.append(("the search asks about fewer changesets than it has",
                       asked < 8, f"{asked} lines"))

        # A second range where one candidate cannot be tested at all.
        root2 = os.path.join(d, "r2")
        os.makedirs(root2)
        sh(["init"], root2)
        a1 = commit(root2, "a one", "1\n")
        commit(root2, "a two broken", "2\n")
        a3 = commit(root2, "a three bad", "3\n")
        a_tip = commit(root2, "a four bad", "4\n")
        sh(["bisect", "start", a1, a_tip], root2)
        r = sh(["bisect", "run",
                'test "$(cat f.txt)" = "2" && exit 125; grep -qE "^(3|4)$" f.txt && exit 1 || exit 0'],
               root2)
        out2 = r.stdout + r.stderr
        checks.append(("a skipped candidate is dropped and the real culprit is still named",
                       a3 in out2 and "skipped" in out2, out2[-220:]))

        # The tree has to be usable afterwards.
        after = head_of(root2)
        checks.append(("the working tree is back at the branch head",
                       after == a_tip, f"{after} vs {a_tip}"))

        # A command that cannot start is not an answer about the changeset.
        root3 = os.path.join(d, "r3")
        os.makedirs(root3)
        sh(["init"], root3)
        b1 = commit(root3, "b one", "1\n")
        commit(root3, "b two", "2\n")
        commit(root3, "b three", "3\n")
        b4 = commit(root3, "b four", "4\n")
        sh(["bisect", "start", b1, b4], root3)
        r = sh(["bisect", "run", "/nonexistent/binary/that/cannot/run"], root3)
        out3 = (r.stdout + r.stderr).lower()
        checks.append(("a command that cannot start is an error, not a verdict",
                       "could not" in out3 or "not found" in out3 or "cannot" in out3, out3[-180:]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} bisect-run checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
