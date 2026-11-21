#!/usr/bin/env python3
"""Does a rename survive where it should, and stay absent where git also says no?

The detector scores a deletion and an addition by shared lines against a 50% threshold. Two things
matter and they pull in opposite directions:

  it must find a rename             an exact-content rename, a rename with a light edit
  it must not invent one            an unrelated file taking the old path's place

There is also a case where finding nothing is correct: a rename plus a rewrite, where the shared lines
fall below half. git's own `log --follow` reports nothing for that shape - measured on a 20-line file
renamed while keeping 5, git shows one commit and not two - so a detector that reported it would be
claiming more than the tool it aims to match.

The exact-content pass runs first and is the part that is not a heuristic: the same hash is the same
file, whatever the line ratio says. That is what makes a rename with no edit reliable rather than a
rounding outcome.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ZF = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta")


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def commit(cwd, message):
    made = sh(["changeset", "create", message, "--author", "t", "--json"], cwd)
    m = re.search(r"(cs[0-9a-f]+)", made.stdout)
    if m:
        sh(["changeset", "commit", m.group(1)], cwd)
    return m.group(1) if m else None


def chain(cwd, path):
    r = sh(["history", path], cwd)
    out = r.stdout + r.stderr
    m = re.search(r"following renames:\s*(.+)", out)
    return [p.strip() for p in m.group(1).split("<-")] if m else []


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="rename-")
    try:
        sh(["init"], d)

        # 1. Exact content, new name. This is a rename with no judgement in it.
        with open(os.path.join(d, "small.txt"), "w") as fh:
            fh.write("a\nb\nc\n")
        commit(d, "add small")
        os.rename(os.path.join(d, "small.txt"), os.path.join(d, "moved.txt"))
        commit(d, "rename small")
        c = chain(d, "moved.txt")
        checks.append(("an exact-content rename is followed back", "small.txt" in c, f"chain={c}"))

        # 2. A 40-line file renamed with one line added: 97% shared, well over the threshold.
        with open(os.path.join(d, "large.txt"), "w") as fh:
            fh.write("".join(f"line {i}\n" for i in range(40)))
        commit(d, "add large")
        os.rename(os.path.join(d, "large.txt"), os.path.join(d, "renamed.txt"))
        with open(os.path.join(d, "renamed.txt"), "a") as fh:
            fh.write("one more line\n")
        commit(d, "rename large and add a line")
        c = chain(d, "renamed.txt")
        checks.append(("a rename with a one-line edit is followed back", "large.txt" in c, f"chain={c}"))

        # 3. An unrelated file under a new name is not a rename.
        with open(os.path.join(d, "before.txt"), "w") as fh:
            fh.write("alpha\nbeta\ngamma\n")
        commit(d, "add before")
        os.remove(os.path.join(d, "before.txt"))
        with open(os.path.join(d, "after.txt"), "w") as fh:
            fh.write("completely\ndifferent\ncontent\n")
        commit(d, "unrelated add")
        c = chain(d, "after.txt")
        checks.append(("an unrelated file is not called a rename", "before.txt" not in c, f"chain={c}"))

        # 4. Rename plus heavy rewrite: below the threshold, and git agrees there is nothing to find.
        with open(os.path.join(d, "orig.txt"), "w") as fh:
            fh.write("".join(f"line {i}\n" for i in range(20)))
        commit(d, "add orig")
        os.rename(os.path.join(d, "orig.txt"), os.path.join(d, "rewritten.txt"))
        with open(os.path.join(d, "rewritten.txt"), "w") as fh:
            fh.write("".join(f"line {i}\n" for i in range(5)))
            fh.write("".join(f"new {i}\n" for i in range(15)))
        commit(d, "rename and rewrite")
        c = chain(d, "rewritten.txt")
        checks.append(
            ("a rename below the similarity threshold is a delete and an add, as in git",
             "orig.txt" not in c,
             f"chain={c}"),
        )

        # 5. Blame still answers across the cases that are detected.
        r = sh(["blame", "renamed.txt"], d)
        checks.append(("blame answers on a renamed path", r.returncode == 0, (r.stderr or "")[:120]))

    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} rename checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
