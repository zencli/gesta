#!/usr/bin/env python3
"""Compare the tool against git and subversion on the operations repeated daily.

Same tree, same edits, same machine, same filesystem (a local path, not a bind
mount). Every number is a median of repeated warm runs.

Run: python3 bench/three_way.py
"""

import os
import re
import shutil
import statistics
import subprocess
import sys
import time

# Found relative to this file. A benchmark that only runs in one container is not a
# measurement another machine can repeat.
_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GESTA = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/three_way"
results = []


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd)


def timeit(cmd, cwd, runs=7):
    sh(cmd, cwd)
    sh(cmd, cwd)
    samples = []
    for _ in range(runs):
        t0 = time.perf_counter()
        r = sh(cmd, cwd)
        if r.returncode != 0:
            return None
        samples.append((time.perf_counter() - t0) * 1000)
    return statistics.median(samples)


def build_tree(root, n):
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    for i in range(n):
        d = os.path.join(root, f"pkg{i // 100}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.py"), "w") as fh:
            fh.write(f"def f{i}():\n    return {i}\n")


def record(name, g, git, svn, detail=""):
    row = {"op": name, "gesta": g, "git": git, "svn": svn, "detail": detail}
    results.append(row)
    cells = []
    for v in (g, git, svn):
        cells.append(f"{v:7.1f}" if v is not None else "   n/a ")
    print(f"  {name:30} gesta {cells[0]} | git {cells[1]} | svn {cells[2]}")


def commit_gesta(root):
    o = sh(f"{GESTA} changeset create snap --author bench", cwd=root)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if m:
        sh(f"{GESTA} changeset commit {m.group(1)}", cwd=root)


def main():
    global GESTA
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")
    # This bench compares three tools by timing them. A runner without one of them made the whole
    # step fail with FileNotFoundError, which reads as a defect in this project and is nothing of the
    # kind: the comparison simply cannot be made there. Skipping is the honest answer, and an exit of
    # 0 with a printed reason is how every other bench reports "not applicable here".
    missing = [t for t in ("git", "svn", "svnadmin") if shutil.which(t) is None]
    if missing:
        print(f"0/0 (skipped: {', '.join(missing)} not installed, so there is nothing to compare)")
        return 0
    # Local copy of the binary, so nothing is measured through a bind mount.
    local = "/tmp/gesta_three_way"
    if not os.path.exists(local):
        shutil.copy2(GESTA, local)
        os.chmod(local, 0o755)
    GESTA = local

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    for n in (500, 2000):
        print(f"\n=== {n} files ===")
        g_dir = os.path.join(ROOT, f"gesta{n}")
        git_dir = os.path.join(ROOT, f"git{n}")
        svn_dir = os.path.join(ROOT, f"svn{n}")
        build_tree(g_dir, n)
        build_tree(git_dir, n)
        build_tree(svn_dir, n)

        # git
        sh("git init -q -b main", cwd=git_dir)
        sh("git config user.email a@b.c && git config user.name bench", cwd=git_dir)
        sh("git add -A && git commit -qm init", cwd=git_dir)
        # gesta
        sh(f"{GESTA} init", cwd=g_dir)
        commit_gesta(g_dir)
        # svn: needs a repository and a checkout
        svn_repo = os.path.join(ROOT, f"svnrepo{n}")
        sh(f"svnadmin create {svn_repo}")
        inner = os.path.join(svn_dir, "wc")
        os.makedirs(inner, exist_ok=True)
        for item in os.listdir(svn_dir):
            if item != "wc":
                shutil.move(os.path.join(svn_dir, item), os.path.join(inner, item))
        sh(f"svn import -q -m init {inner} file://{svn_repo}/trunk")
        shutil.rmtree(svn_dir)
        sh(f"svn checkout -q file://{svn_repo}/trunk {svn_dir}")

        def touch(root):
            p = os.path.join(root, "pkg0", "f0.py")
            with open(p, "a") as fh:
                fh.write("# edit\n")

        # status, clean
        record(
            "status, clean",
            timeit(f"{GESTA} status", g_dir),
            timeit("git status --porcelain", git_dir),
            timeit("svn status", svn_dir),
        )

        # status, one file edited
        for d in (g_dir, git_dir, svn_dir):
            touch(d)
        record(
            "status, 1 file edited",
            timeit(f"{GESTA} status", g_dir),
            timeit("git status --porcelain", git_dir),
            timeit("svn status", svn_dir),
        )

        # diff
        record(
            "diff, 1 file",
            timeit(f"{GESTA} diff", g_dir),
            timeit("git diff", git_dir),
            timeit("svn diff", svn_dir),
        )

        # commit one file
        def bench(fn, root):
            samples = []
            for _ in range(5):
                touch(root)
                t0 = time.perf_counter()
                fn(root)
                samples.append((time.perf_counter() - t0) * 1000)
            return statistics.median(samples)

        record(
            "commit, 1 file",
            bench(commit_gesta, g_dir),
            bench(lambda r: sh("git add -A && git commit -qm tick", cwd=r), git_dir),
            bench(lambda r: sh("svn commit -q -m tick", cwd=r), svn_dir),
        )

        # log
        record(
            "log, last 10",
            timeit(f"{GESTA} log --limit 10", g_dir),
            timeit("git log --oneline -10", git_dir),
            timeit("svn log -l 10 -q", svn_dir),
        )

        # blame
        record(
            "blame, small file",
            timeit(f"{GESTA} blame pkg0/f0.py", g_dir),
            timeit("git blame pkg0/f0.py", git_dir),
            timeit("svn blame pkg0/f0.py", svn_dir),
        )

    print()
    for tool in ("git", "svn"):
        wins = sum(
            1
            for r in results
            if r["gesta"] is not None and r[tool] is not None and r["gesta"] < r[tool]
        )
        total = sum(1 for r in results if r["gesta"] is not None and r[tool] is not None)
        print(f"gesta faster than {tool} on {wins}/{total} operations")
    return 0


if __name__ == "__main__":
    sys.exit(main())
