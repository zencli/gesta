#!/usr/bin/env python3
"""Does every declared hook point actually run, and does a refusing hook actually refuse?

A hook list is easy to declare and easy to leave unconnected, and the failure is silent in the worst
way: a repository installs a `pre-push` guard, `gesta hook list` reports it installed, and the guard
never runs. So each check installs a real program that appends its own name to a file, performs the
action, and counts that name before and after.

The second half is the part that matters for safety: a hook that exits non-zero has to stop the action.
A `pre-commit` that complains and commits anyway is worse than no hook, because the repository now
believes it is protected.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))

POINTS = ("pre-changeset", "pre-commit", "post-commit", "pre-land", "post-checkout", "pre-push")


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def install(root, name, body):
    return subprocess.run([ZF, "hook", "install", name], cwd=root, input=body,
                          capture_output=True, text=True)


def marks(path):
    if not os.path.isfile(path):
        return []
    with open(path) as fh:
        return [line.strip() for line in fh if line.strip()]


def new_repo(root, mark=None):
    """A repository, optionally with every hook point installed as a marker."""
    os.makedirs(root, exist_ok=True)
    sh(["init"], root)
    installed = []
    if mark:
        for name in POINTS:
            body = f"#!/bin/sh\necho {name} >> '{mark}'\nexit 0\n"
            installed.append((name, install(root, name, body).returncode == 0))
    return installed


def touch(root, rel, content):
    with open(os.path.join(root, rel), "w") as fh:
        fh.write(content)


def create(root, name):
    r = sh(["changeset", "create", name, "--author", "t", "--json"], root)
    m = re.search(r"cs[0-9a-f]+", r.stdout)
    return (m.group(0) if m else None), r


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="hooks-")
    try:
        mark = os.path.join(d, "marks.txt")
        root = os.path.join(d, "r")
        installed = new_repo(root, mark)

        checks.append(("every declared hook point installs",
                       len(installed) == len(POINTS) and all(ok for _, ok in installed),
                       str([n for n, ok in installed if not ok])))

        listed = sh(["hook", "list"], root).stdout
        checks.append(("the listed set matches what installs",
                       all(n in listed for n, _ in installed), listed[:240]))

        # A baseline commit, so the history has somewhere to start.
        touch(root, "f.txt", "1\n")
        cs, _ = create(root, "seed")
        if cs:
            sh(["changeset", "commit", cs], root)

        # pre-changeset: the snapshot.
        n0 = marks(mark).count("pre-changeset")
        touch(root, "a.txt", "one\n")
        cs_one, r = create(root, "one")
        checks.append(("pre-changeset runs when a snapshot is taken",
                       marks(mark).count("pre-changeset") > n0, (r.stdout + r.stderr)[-160:]))

        # pre-commit and post-commit: the snapshot becomes a changeset.
        n0 = marks(mark).count("pre-commit")
        n1 = marks(mark).count("post-commit")
        if cs_one:
            sh(["changeset", "commit", cs_one], root)
        checks.append(("pre-commit runs when a changeset is committed",
                       marks(mark).count("pre-commit") > n0, str(marks(mark)[-6:])))
        checks.append(("post-commit runs after it landed",
                       marks(mark).count("post-commit") > n1, str(marks(mark)[-6:])))

        # post-checkout: the tree moves to another branch.
        sh(["branch", "create", "other"], root)
        n0 = marks(mark).count("post-checkout")
        sh(["branch", "switch", "other"], root)
        checks.append(("post-checkout runs when the tree moves to another branch",
                       marks(mark).count("post-checkout") > n0, str(marks(mark)[-6:])))

        # pre-land: a merge onto a shared target. The branch needs a commit the target lacks, or the
        # merge returns "nothing to merge" before any hook could run - which is correct behaviour and
        # a useless test.
        touch(root, "on-other.txt", "from the other branch" + chr(10))
        cs_other, _ = create(root, "a change on other")
        if cs_other:
            sh(["changeset", "commit", cs_other], root)
        n0 = marks(mark).count("pre-land")
        r = sh(["branch", "merge", "other", "--target", "main"], root)
        checks.append(("pre-land runs before a landing",
                       marks(mark).count("pre-land") > n0,
                       f"marks={marks(mark)[-6:]} out={(r.stdout + r.stderr)[-140:]}"))

        # pre-push: a refusing hook has to stop it.
        install(root, "pre-push", "#!/bin/sh\necho 'refused by pre-push' >&2\nexit 1\n")
        r = sh(["remote", "push", "origin", "--branch", "other"], root)
        out = r.stdout + r.stderr
        checks.append(("a refusing pre-push stops the push and says which hook refused",
                       r.returncode != 0 and "pre-push" in out, out[-220:]))

        # A refusing pre-commit keeps the changeset out of history.
        d2 = os.path.join(d, "r2")
        new_repo(d2)
        touch(d2, "f.txt", "1\n")
        cs, _ = create(d2, "seed")
        if cs:
            sh(["changeset", "commit", cs], d2)
        install(d2, "pre-commit", "#!/bin/sh\necho 'refused by pre-commit' >&2\nexit 1\n")
        touch(d2, "c.txt", "three\n")
        cs2, _ = create(d2, "should not land")
        out = ""
        if cs2:
            r = sh(["changeset", "commit", cs2], d2)
            out = r.stdout + r.stderr
        log = sh(["log", "--json"], d2).stdout + sh(["log"], d2).stdout
        # `log` lists pending changesets too, so presence proves nothing. What matters is that the
        # refused one is still pending: the hook stopped the commit, it did not delete the work.
        refused_still_pending = "should not land" in log and "pending" in log
        checks.append(("a refusing pre-commit keeps the changeset out of history",
                       refused_still_pending and "refused by pre-commit" in out,
                       out[-220:]))

        # A repository with no hooks at all still works.
        d3 = os.path.join(d, "r3")
        new_repo(d3)
        touch(d3, "d.txt", "four\n")
        r = sh(["changeset", "create", "no hooks", "--author", "t", "--json"], d3)
        checks.append(("a repository with no hooks installed still works",
                       r.returncode == 0, (r.stdout + r.stderr)[-180:]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} hook checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
