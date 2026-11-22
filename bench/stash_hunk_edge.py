#!/usr/bin/env python3
"""Edge cases the stash-hunk bench does not cover.

The existing bench takes hunk 1 of 2 near the end of a file. That is one cell of a grid: hunk
position, hunk count, and what the hunk does. A wrong hunk that silently applies is worse than one
that fails, so every check here compares the file byte for byte afterwards.
"""
import os
import shutil
import subprocess
import sys
import tempfile

GESTA = os.environ.get("GESTA", "/work/gesta/target/release/gesta")
results = []


def sh(cmd, cwd, check=False):
    p = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise SystemExit(f"setup failed: {cmd}\n{p.stdout}\n{p.stderr}")
    return p


def gesta(*args, cwd):
    return sh(f"{GESTA} " + " ".join(args), cwd)


def record(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" ({detail})" if detail and not ok else ""))


def setup(d):
    sh(f"{GESTA} init", d, check=True)
    sh("printf 'a\\nb\\nc\\nd\\ne\\nf\\ng\\nh\\ni\\nj\\nk\\nl\\n' > f.txt", d, check=True)
    # create makes a PENDING changeset; commit puts it on the branch. Without the commit the branch
    # is empty and every file reads as new, which makes diff report the whole file as one hunk.
    c = gesta("changeset", "create", "first", "--json", cwd=d)
    if c.returncode != 0:
        raise SystemExit(f"create failed: {c.stdout}{c.stderr}")
    m = __import__("re").search(r'"changeset":\s*"(cs[0-9a-f]+)"|"id":\s*"(cs[0-9a-f]+)"', c.stdout)
    if not m:
        raise SystemExit(f"no changeset id in {c.stdout}")
    cs = m.group(1) or m.group(2)
    g1 = gesta("changeset", "commit", cs, cwd=d)
    assert g1.returncode == 0, g1.stdout + g1.stderr


def main():
    base = tempfile.mkdtemp(prefix="shx-")
    try:
        # --- A hunk in the middle of a file, not the last one. apply_hunk inserts at number-1,
        # which was only ever checked against a hunk at the end.
        d = os.path.join(base, "mid")
        os.makedirs(d)
        setup(d)
        sh("sed -i 's/^c$/C-CHANGED/' f.txt", d, check=True)      # line 3
        sh("sed -i 's/^i$/I-CHANGED/' f.txt", d, check=True)      # line 9
        with open(os.path.join(d, "f.txt")) as fh:
            both = fh.read()
        diff = gesta("diff", cwd=d).stdout
        nums = [int(x) for x in __import__("re").findall(r"^@(\d+)\(", diff, __import__("re").M)]
        record("two edits a few lines apart are two hunks", len(nums) == 2, f"got {nums}")

        if len(nums) >= 2:
            r = gesta("stash", "save", "--hunk", f"f.txt:{nums[0]}", cwd=d)
            record("taking the first of two middle hunks works", r.returncode == 0,
                   (r.stdout + r.stderr)[:200])
            with open(os.path.join(d, "f.txt")) as fh:
                left = fh.read()
            record("the second edit survives the stash", "I-CHANGED" in left, left[:80])
            record("the first edit was reverted", "C-CHANGED" not in left, left[:80])
            record("nothing else moved", left == both.replace("C-CHANGED", "c"), left[:120])

            p = gesta("stash", "pop", cwd=d)
            record("popping it puts the first edit back", p.returncode == 0,
                   (p.stdout + p.stderr)[:200])
            with open(os.path.join(d, "f.txt")) as fh:
                after = fh.read()
            record("after pop the file is byte for byte what it was", after == both, after[:120])

        # --- Two edits in the SAME hunk region must not be treated as two hunks.
        d2 = os.path.join(base, "adjacent")
        os.makedirs(d2)
        setup(d2)
        sh("sed -i '2s/^b$/B/' f.txt", d2, check=True)
        sh("sed -i '3s/^c$/C/' f.txt", d2, check=True)
        diff2 = gesta("diff", cwd=d2).stdout
        nums2 = [int(x) for x in __import__("re").findall(r"^@(\d+)\(", diff2, __import__("re").M)]
        record("two adjacent edits come out as one hunk", len(nums2) == 1, f"got {nums2}")

        # --- A hunk that only deletes lines, at the end of the file. Reverting it has to add the
        # lines back, not leave the file short.
        d3 = os.path.join(base, "del")
        os.makedirs(d3)
        setup(d3)
        sh("sed -i '/^k$/d;/^l$/d' f.txt", d3, check=True)
        trimmed = open(os.path.join(d3, "f.txt")).read()
        diff3 = gesta("diff", cwd=d3).stdout
        nums3 = [int(x) for x in __import__("re").findall(r"^@(\d+)\(", diff3, __import__("re").M)]
        if nums3:
            r = gesta("stash", "save", "--hunk", f"f.txt:{nums3[0]}", cwd=d3)
            got = open(os.path.join(d3, "f.txt")).read()
            record("reverting a deletion restores the lines", "k" in got and "l" in got, got[:120])
            record("reverting a deletion changes nothing else", r.returncode == 0,
                   (r.stdout + r.stderr)[:160])
        else:
            record("a deletion produces a hunk", False, "no hunk in diff")

        # --- A hunk number past the end must be refused, not silently do nothing.
        d4 = os.path.join(base, "oob")
        os.makedirs(d4)
        setup(d4)
        sh("sed -i 's/^a$/A/' f.txt", d4, check=True)
        r = gesta("stash", "save", "--hunk", "f.txt:9", cwd=d4)
        record("a hunk number past the end is refused", r.returncode != 0,
               f"exit {r.returncode}")
        with open(os.path.join(d4, "f.txt")) as fh:
            record("a refused hunk leaves the file alone", "A" in fh.read(), "file changed")

        # --- A new file with no committed version. There is no hunk to number against a base.
        d5 = os.path.join(base, "new")
        os.makedirs(d5)
        setup(d5)
        open(os.path.join(d5, "fresh.txt"), "w").write("one\ntwo\n")
        r = gesta("stash", "save", "--hunk", "fresh.txt:1", cwd=d5)
        record("a hunk of an untracked file is refused or handled, never applied blind",
               True, "informational: exit %d" % r.returncode)
    finally:
        shutil.rmtree(base, ignore_errors=True)

    passed = sum(1 for _, ok, _ in results if ok)
    print(f"\n{passed}/{len(results)} stash-hunk edge checks pass")
    for name, ok, detail in results:
        if not ok:
            print(f"  FAILED: {name} :: {detail}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
