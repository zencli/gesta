#!/usr/bin/env python3
"""Attach a note to a changeset without changing the changeset.

`git notes` exists because a commit is immutable and the thing you want to say about it usually arrives
later: which ticket it closes, which reviewer signed off, why it was reverted. Editing the commit would
change its hash and break every reference to it, so the note lives beside the commit and points at it.

That the note does not touch the changeset is the first check, and the one that would be easiest to get
wrong: writing into the changeset row would be simpler and would silently rewrite history.

  a note can be attached and read back        the basic round trip
  attaching it does not change the changeset  the hash and message are exactly as they were
  a second note replaces the first             one note per changeset, not a pile
  a note on a changeset that is not there     refused, rather than stored against nothing
  removing a note leaves the changeset         the changeset outlives its annotation
  a note is not part of the checkout          no file appears in the working tree
  the note is not lost by a later commit      history moving on does not drop annotations
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
    d = tempfile.mkdtemp(prefix="notes-")
    try:
        root = os.path.join(d, "r")
        os.makedirs(root)
        sh(["init"], root)
        a = commit(root, "first change", "a.txt", "one\n")

        # The changeset as it was, before any note.
        before = (sh(["log"], root).stdout
                  + sh(["log"], root).stderr)

        r = sh(["notes", "add", a, "-m", "closes TICKET-42"], root)
        out = r.stdout + r.stderr
        checks.append(("a note can be attached", r.returncode == 0, out[-200:]))

        shown = (sh(["notes", "show", a], root).stdout
                 + sh(["notes", "show", a], root).stderr)
        checks.append(("it reads back", "TICKET-42" in shown, shown[-200:]))

        after = (sh(["log"], root).stdout
                 + sh(["log"], root).stderr)
        checks.append(("attaching it does not change the changeset",
                       before.replace(before.split("\n")[0], "") ==
                       after.replace(after.split("\n")[0], ""),
                       f"before={len(before)} after={len(after)}"))

        # One note per changeset.
        sh(["notes", "add", a, "-m", "actually closes TICKET-43"], root)
        shown = (sh(["notes", "show", a], root).stdout
                 + sh(["notes", "show", a], root).stderr)
        checks.append(("a second note replaces the first",
                       "TICKET-43" in shown and "TICKET-42" not in shown, shown[-200:]))

        # A note against nothing.
        r = sh(["notes", "add", "cs000000000000", "-m", "orphan"], root)
        out = (r.stdout + r.stderr).lower()
        checks.append(("a note on a changeset that is not there is refused",
                       r.returncode != 0 and ("no changeset" in out or "not found" in out), out[-200:]))

        # No file in the tree.
        listing = sh(["ls"], root).stdout
        checks.append(("a note is not part of the checkout",
                       "note" not in listing.lower(), listing[:200]))

        # History moving on does not lose it.
        commit(root, "second change", "b.txt", "two\n")
        shown = (sh(["notes", "show", a], root).stdout
                 + sh(["notes", "show", a], root).stderr)
        checks.append(("a later commit does not lose the note",
                       "TICKET-43" in shown, shown[-200:]))

        # Removing leaves the changeset.
        sh(["notes", "remove", a], root)
        gone = (sh(["notes", "show", a], root).stdout
                + sh(["notes", "show", a], root).stderr)
        still = (sh(["log"], root).stdout
                 + sh(["log"], root).stderr)
        checks.append(("removing a note leaves the changeset",
                       "TICKET-43" not in gone and "first change" in still,
                       f"note={'TICKET-43' in gone} cs={'first change' in still}"))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} notes checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
