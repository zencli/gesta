#!/usr/bin/env python3
"""Do submodules clone at their pin, report their state, and leave the parent clean?

The parent's walk skips a submodule checkout, so the child's files must never land in the parent's
tree. That property is the reason nested repositories are dangerous, and it is what the last check
here measures rather than assumes.
"""
import os
import shutil
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=180)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    base = tempfile.mkdtemp(prefix="submodule-")
    child = os.path.join(base, "child")
    parent = os.path.join(base, "parent")
    try:
        # A child repository with two commits, so a pin can name the older one.
        os.makedirs(child)
        sh("git init -q && git config user.email t@t.c && git config user.name t", child)
        with open(os.path.join(child, "child.txt"), "w") as fh:
            fh.write("v1\n")
        sh("git add -A && git commit -qm v1", child)
        v1 = sh("git rev-parse HEAD", child)[1].strip()
        with open(os.path.join(child, "child.txt"), "w") as fh:
            fh.write("v1\nv2\n")
        sh("git add -A && git commit -qm v2", child)

        # A parent repository with one committed file.
        os.makedirs(parent)
        sh(f"{ZF} init", parent)
        with open(os.path.join(parent, "p.txt"), "w") as fh:
            fh.write("parent\n")
        out = sh(f'{ZF} changeset create "init" --author t --json', parent)[1]
        import re

        m = re.search(r"(cs[0-9a-f]+)", out)
        if m:
            sh(f"{ZF} changeset commit {m.group(1)}", parent)

        added = "added submodule" in sh(f"{ZF} submodule add vendor/child {child}", parent)[1]

        # Pin to the older commit, written the way a person would: replace whatever pin `add`
        # recorded with the short id from git log. `add` records the child's head, so a plain append
        # leaves two `commit` lines and the first one wins - which is what this bench did before, and
        # it read as "init ignored the pin" when the file simply said two things.
        gm = os.path.join(parent, ".gitmodules")
        with open(gm) as fh:
            lines = fh.read().splitlines()
        lines = [ln for ln in lines if not ln.strip().startswith("commit =")]
        with open(gm, "w") as fh:
            out_lines = []
            for ln in lines:
                out_lines.append(ln)
                if ln.strip().startswith(f"url = {child}"):
                    out_lines.append(f"    commit = {v1[:7]}")
            fh.write("\n".join(out_lines) + "\n")

        init_out = sh(f"{ZF} submodule init", parent)[1]

        # The checkout is at the pin, not at the child's head.
        got = ""
        cp = os.path.join(parent, "vendor", "child", "child.txt")
        if os.path.exists(cp):
            with open(cp) as fh:
                got = fh.read().strip()
        at_pin = got == "v1"

        status = sh(f"{ZF} submodule status", parent)[1]
        says_clean = "clean" in status

        # A missing checkout is reported as missing rather than as a deletion.
        shutil.rmtree(os.path.join(parent, "vendor"), ignore_errors=True)
        missing_status = sh(f"{ZF} submodule status", parent)[1]

        # Back to present, then the property that matters: the child's files are not the parent's.
        sh(f"{ZF} submodule init", parent)
        parent_status = sh(f"{ZF} status", parent)[1]
        leaking = "vendor/child/child.txt" in parent_status

        checks = [
            ("add wrote a .gitmodules entry", added),
            ("init cloned the submodule", "cloned" in init_out),
            ("the checkout is at the pinned commit, not the head", at_pin),
            ("status reports it clean when it matches the pin", says_clean),
            ("a missing checkout is reported as missing", "missing" in missing_status),
            ("the parent never records the child's files", not leaking),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} submodule checks pass")
            return 0
        print(f"child.txt={got!r} v1={v1[:7]}")
        print(status.strip())
        return 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
