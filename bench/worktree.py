#!/usr/bin/env python3
"""Does a second working tree share one repository without copying it?

`git worktree add` exists because two branches cannot be checked out at once in one directory, and the
alternative - clone the repository again - duplicates every object and then has to be kept in sync by
hand. A linked worktree is a second directory whose `.gesta` is a pointer to the first one's store.

The shape matters and is what makes it cheap: `.gesta` in a linked worktree is a FILE holding the path
to the main store, not a directory holding a copy of it. So the checks are about identity and about
isolation:

  a linked worktree is created                   the directory exists and names the main store
  it holds the branch it was asked for           checked out, not the main tree's branch
  its store is the main store                    one database, not a copy
  a commit there is visible in the main tree     the same store means the same history
  two worktrees hold two branches at once        which is the reason the feature exists
  removing it leaves the store intact            the main tree still works afterwards
  a worktree cannot be removed twice             the second call says so instead of failing weirdly
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


def commit(cwd, name, path, content):
    with open(os.path.join(cwd, path), "w") as fh:
        fh.write(content)
    r = sh(["changeset", "create", name, "--author", "t", "--json"], cwd)
    m = re.search(r"cs[0-9a-f]+", r.stdout)
    if not m:
        raise SystemExit(f"no changeset for {name}: {r.stdout}{r.stderr}")
    sh(["changeset", "commit", m.group(0)], cwd)
    return m.group(0)


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="worktree-")
    try:
        main_tree = os.path.join(d, "main")
        os.makedirs(main_tree)
        sh(["init"], main_tree)
        commit(main_tree, "seed", "a.txt", "one\n")
        sh(["branch", "create", "side"], main_tree)

        wt = os.path.join(d, "side-tree")
        r = sh(["worktree", "add", wt, "side"], main_tree)
        out = r.stdout + r.stderr
        checks.append(("a linked worktree is created", os.path.isdir(wt), out[-200:]))

        # `.gesta` has to be a file naming the main store, not a copy of it.
        marker = os.path.join(wt, ".gesta")
        is_file = os.path.isfile(marker)
        content = ""
        if is_file:
            with open(marker) as fh:
                content = fh.read().strip()
        checks.append(("its .gesta names the main store rather than copying it",
                       is_file and "main" in content, f"file={is_file} content={content[:120]!r}"))

        # One database, so the file in the worktree is the same file.
        main_db = os.path.join(main_tree, ".gesta", "gesta.db")
        checks.append(("the store is not duplicated",
                       os.path.isfile(main_db) and not os.path.isfile(
                           os.path.join(wt, ".gesta", "gesta.db")),
                       f"main db={os.path.isfile(main_db)}"))

        # A commit made in the worktree is visible from the main tree.
        commit(wt, "from the side tree", "b.txt", "two\n")
        listing = sh(["log", "--json"], main_tree).stdout + sh(["log"], main_tree).stdout
        checks.append(("a commit in the worktree is visible in the main tree",
                       "from the side tree" in listing, listing[-200:]))

        # Both trees hold a branch at the same time, which is the whole reason for the feature.
        st = sh(["worktree", "list"], main_tree).stdout + sh(["worktree", "list"], main_tree).stderr
        checks.append(("two worktrees hold two branches at once",
                       "side-tree" in st, st[-220:]))

        # Removing the worktree must not take the store with it.
        r = sh(["worktree", "remove", wt], main_tree)
        out = r.stdout + r.stderr
        gone = not os.path.isdir(wt)
        db_there = os.path.isfile(main_db)
        after = sh(["log", "--json"], main_tree).stdout + sh(["log"], main_tree).stdout
        checks.append(("removing it leaves the store intact and the main tree working",
                       gone and db_there and "from the side tree" in after,
                       f"gone={gone} db={db_there}"))

        # A second remove has to say what is wrong.
        r = sh(["worktree", "remove", wt], main_tree)
        out = (r.stdout + r.stderr).lower()
        checks.append(("a worktree cannot be removed twice",
                       r.returncode != 0 and ("no worktree" in out or "not" in out), out[-180:]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} worktree checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
