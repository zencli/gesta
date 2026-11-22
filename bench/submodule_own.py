#!/usr/bin/env python3
"""Can a submodule point at another repository of THIS kind, and not only at a git one?

The existing submodule path shells out to `git clone` and `git checkout`. That works when the child is
a git repository and fails with `fatal: repository ... does not exist` when the child is one of ours,
which is the common case for a project that uses this tool for its own children.

The check that matters is the negative one from git's design: a submodule is a **pointer**, not a copy.
Recording the child's files in the parent would duplicate them, and then the two copies drift with no
way to tell which is right. So the parent's tree must hold one entry for the path and none of the
child's files, and the entry must name the child's commit.

  a submodule of a repository of this kind clones            it does not need git
  the parent records a pointer, not the child's files        no a.txt in the parent's tree
  the pointer names the child's commit                       the pin is the child head at add time
  init materialises the child's worktree                     the files appear inside the path
  status reports the pin as matching                         not "unpinned" or "missing"
  a moved child head is reported as differing                the pin is not silently forgotten
  update returns the child to the pin                        which is what pinning is for
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
    d = os.path.dirname(p)
    if d:
        os.makedirs(d, exist_ok=True)
    with open(p, "w") as fh:
        fh.write(content)


def repo(path, message, filename, content):
    os.makedirs(path, exist_ok=True)
    sh(["init"], path)
    write(path, filename, content)
    r = sh(["changeset", "create", message, "--author", "t", "--json"], path)
    m = re.search(r"(cs[0-9a-f]+)", r.stdout)
    if not m:
        raise SystemExit(f"no changeset in {path}: {r.stdout}{r.stderr}")
    sh(["changeset", "commit", m.group(1)], path)
    return m.group(1)


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="submodule-")
    try:
        child = os.path.join(d, "child")
        child_head = repo(child, "child one", "a.txt", "child\n")

        parent = os.path.join(d, "parent")
        repo(parent, "parent one", "b.txt", "parent\n")

        # Adding records the pointer; init does the materialising.
        r = sh(["submodule", "add", "sub", child], parent)
        out = r.stdout + r.stderr
        checks.append(("a submodule pointing at a repository of this kind is accepted",
                       r.returncode == 0, out[-240:]))

        r = sh(["submodule", "init"], parent)
        out = r.stdout + r.stderr
        cloned = os.path.isdir(os.path.join(parent, "sub"))
        checks.append(("init materialises it without needing git",
                       r.returncode == 0 and cloned, out[-280:]))

        child_file = os.path.join(parent, "sub", "a.txt")
        checks.append(("the child's file is inside the path",
                       os.path.isfile(child_file),
                       "a.txt not found under sub/" if not os.path.isfile(child_file) else ""))

        # The pointer, not a copy: the parent's own tree must not hold the child's file.
        ls = sh(["ls"], parent).stdout
        pointer_only = "a.txt" not in ls
        checks.append(("the parent records a pointer and not the child's files",
                       pointer_only, ls[:240]))

        # The pin names the child's commit.
        status = sh(["submodule", "status"], parent).stdout + sh(["submodule", "status"], parent).stderr
        checks.append(("the pointer names the child's commit",
                       child_head in status, status[:240]))
        checks.append(("status does not report the pin as unpinned or missing",
                       "unpinned" not in status and "missing" not in status, status[:240]))

        # A moved child head has to be visible as a difference, or the pin means nothing.
        repo(child, "child two", "c.txt", "more\n")
        status2 = sh(["submodule", "status"], parent).stdout + sh(["submodule", "status"], parent).stderr
        checks.append(("a child that moved is reported as differing",
                       "differ" in status2.lower() or "behind" in status2.lower(),
                       status2[:260]))

        # Update puts it back where the parent says it belongs.
        r = sh(["submodule", "update"], parent)
        out = r.stdout + r.stderr
        still_has_c = os.path.isfile(os.path.join(parent, "sub", "c.txt"))
        checks.append(("update returns the child to the pinned commit",
                       r.returncode == 0 and not still_has_c,
                       f"rc={r.returncode} c.txt={still_has_c} {out[-200:]}"))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} submodule checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
