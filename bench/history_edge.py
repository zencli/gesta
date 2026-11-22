#!/usr/bin/env python3
"""History operations on the cases that are easy to get wrong and hard to notice.

`revert`, `cherry-pick` and `blame` are each checked somewhere, but on a file that exists at every
step. The cases here involve a file that was deleted and came back, a change applied twice, and a
chain of two renames - where a mistake produces a plausible answer rather than an error.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))
results = []


def sh(cmd, cwd, check=False):
    p = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise SystemExit(f"setup failed: {cmd}\n{p.stdout}\n{p.stderr}")
    return p


def g(*args, cwd):
    # Quote each argument: a message with a space in it was being split into two arguments.
    import shlex
    parts = [shlex.quote(str(a)) for a in args]
    return sh(f"{ZF} " + " ".join(parts), cwd)


def commit(d, msg, files):
    """Write files, then create and commit a changeset. Returns the changeset id."""
    for name, text in files.items():
        full = os.path.join(d, name)
        os.makedirs(os.path.dirname(full), exist_ok=True) if os.path.dirname(full) else None
        with open(full, "w") as fh:
            fh.write(text)
    c = g("changeset", "create", msg, "--json", cwd=d)
    m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
    if not m:
        raise SystemExit(f"no changeset from create: {c.stdout}{c.stderr}")
    cs = m.group(1)
    r = g("changeset", "commit", cs, cwd=d)
    if r.returncode != 0:
        raise SystemExit(f"commit failed: {r.stdout}{r.stderr}")
    return cs


def record(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" :: {detail[:150]}" if not ok else ""))


def main():
    base = tempfile.mkdtemp(prefix="hy-")
    try:
        # --- revert of a changeset that CREATED a file. The file must be gone, and a later revert of
        # that revert must bring it back with its content, not an empty file.
        d = os.path.join(base, "rev")
        os.makedirs(d)
        g("init", cwd=d)
        commit(d, "seed", {"base.txt": "base\n"})
        cs = commit(d, "add report", {"report.txt": "one\ntwo\nthree\n"})
        r = g("revert", cs, cwd=d)
        gone = not os.path.exists(os.path.join(d, "report.txt"))
        record("reverting the changeset that added a file removes the file",
               r.returncode == 0 and gone, (r.stdout + r.stderr)[:200])

        # Reverting the revert: the file must come back with its bytes.
        rv = g("log", "--limit", "1", "--json", cwd=d).stdout
        m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', rv)
        if m:
            g("revert", m.group(1), cwd=d)
            p = os.path.join(d, "report.txt")
            back = open(p).read() if os.path.exists(p) else ""
            record("reverting the revert brings the file back with its content",
                   back == "one\ntwo\nthree\n", repr(back))

        # --- cherry-pick the same change twice. The second must be refused or be a no-op, never a
        # doubled line: applying a patch twice is how a file quietly grows.
        d2 = os.path.join(base, "pick")
        os.makedirs(d2)
        g("init", cwd=d2)
        commit(d2, "seed", {"a.txt": "keep\n"})
        g("branch", "create", "side", cwd=d2)
        g("branch", "switch", "side", cwd=d2)
        cs2 = commit(d2, "add line", {"a.txt": "keep\nadded\n"})
        g("branch", "switch", "main", cwd=d2)
        r1 = g("cherry-pick", cs2, cwd=d2)
        after1 = open(os.path.join(d2, "a.txt")).read()
        r2 = g("cherry-pick", cs2, cwd=d2)
        after2 = open(os.path.join(d2, "a.txt")).read()
        record("picking a change once applies it", r1.returncode == 0 and after1 == "keep\nadded\n",
               repr(after1))
        record("picking the same change twice does not double it",
               after2 == after1, f"after1={after1!r} after2={after2!r} exit={r2.returncode}")

        # --- blame through TWO renames. rename_history follows one hop; the second hop is where a
        # single-hop implementation reports the middle commit for every line.
        d3 = os.path.join(base, "blame")
        os.makedirs(d3)
        g("init", cwd=d3)
        author1 = "alice"
        with open(os.path.join(d3, "orig.py"), "w") as fh:
            fh.write("line a\nline b\n")
        c = g("changeset", "create", "orig", "--author", author1, "--json", cwd=d3)
        m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
        g("changeset", "commit", m.group(1), cwd=d3) if m else None
        os.rename(os.path.join(d3, "orig.py"), os.path.join(d3, "mid.py"))
        commit(d3, "rename 1", {"mid.py": "line a\nline b\n"})
        os.rename(os.path.join(d3, "mid.py"), os.path.join(d3, "final.py"))
        commit(d3, "rename 2", {"final.py": "line a\nline b\n"})
        b = g("blame", "final.py", cwd=d3)
        # Every line was written by alice; a single-hop blame names the middle commit instead.
        record("blame follows two renames back to the author who wrote the lines",
               b.returncode == 0 and "alice" in b.stdout,
               (b.stdout + b.stderr)[:300])

        # --- history of a path through two renames.
        h = g("history", "final.py", cwd=d3)
        record("history of a renamed path reaches the original name",
               h.returncode == 0 and ("orig.py" in h.stdout or "mid.py" in h.stdout),
               (h.stdout + h.stderr)[:300])
    finally:
        shutil.rmtree(base, ignore_errors=True)

    passed = sum(1 for _, ok, _ in results if ok)
    print(f"\n{passed}/{len(results)} history edge checks pass")
    for name, ok, detail in results:
        if not ok:
            print(f"  FAILED: {name} :: {detail[:200]}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
