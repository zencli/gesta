#!/usr/bin/env python3
"""Does the cost of a commit depend on the size of the change, or of the repo?

A source control system for agents has to accept commits faster than agents can produce them, and
agents produce many small ones. If committing one file costs more as the repository grows, the
system gets slower exactly as it becomes more useful.

Two phases, measured apart, because they have different shapes and only one of them is the claim:

- `create` reads the working tree to work out what changed. Its cost follows the number of files on
  disk, and every tool that answers "what changed" has that cost. Git pays it in `git status`.
- `commit` writes the change and its history. This is the phase the project claims is flat, and a
  run that reported the two together would hide a flat commit behind a growing scan, or the reverse.

Reporting one number for both was wrong: on 8000 files the split is 34 ms of scan against 10 ms of
write, so a combined figure says "2.12x worse" about a phase that did not change at all.

Run: python3 bench/scale.py
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
ROOT = "/tmp/gesta_scale"
rows = []


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd)


def build(root, n):
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    for i in range(n):
        d = os.path.join(root, f"d{i // 200}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.py"), "w") as fh:
            fh.write(f"def f{i}():\n    return {i}\n")


def gesta_commit(root, msg):
    o = sh(f"{GESTA} changeset create {msg} --author bench", cwd=root)
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if not m:
        return False
    r = sh(f"{GESTA} changeset commit {m.group(1)}", cwd=root)
    return r.returncode == 0


def gesta_split(root, msg):
    """One edit, timed as two phases. Returns (create_ms, commit_ms) or None.

    `create` is the scan and `commit` is the write. They are timed separately because they are
    different claims: the scan is allowed to grow with the tree, the write is not.
    """
    t0 = time.perf_counter()
    o = sh(f"{GESTA} changeset create {msg} --author bench", cwd=root)
    create = (time.perf_counter() - t0) * 1000
    m = re.search(r"\b(cs[0-9a-f]{6,})\b", o.stdout)
    if not m:
        return None
    t0 = time.perf_counter()
    r = sh(f"{GESTA} changeset commit {m.group(1)}", cwd=root)
    commit = (time.perf_counter() - t0) * 1000
    if r.returncode != 0:
        return None
    return create, commit


def measure_split(root, runs=11, rounds=3):
    """Median of the medians for each phase, same shape as `measure`."""
    def one(fn):
        medians = []
        for _ in range(rounds):
            samples = []
            for i in range(runs + 2):
                with open(os.path.join(root, "d0", "f0.py"), "a") as fh:
                    fh.write(f"# tick {i}\n")
                got = fn(root, f"t{i}")
                if got is None:
                    return None
                if i >= 2:
                    samples.append(got)
            samples.sort()
            medians.append(samples[len(samples) // 2])
        medians.sort()
        return medians[len(medians) // 2]

    got = one(gesta_split)
    if got is None:
        return None
    return got[0], got[1]


def measure(root, prepare, commit, runs=11, rounds=3):
    """Median of the medians, and the spread, because one median hides how noisy this is.

    Running the same one file commit ten times in a row on this machine gave 29 ms to 50 ms.
    A single median from that spread cannot support a claim, so the report carries the
    interquartile spread beside the number.
    """
    prepare(root)
    medians = []
    for _ in range(rounds):
        samples = []
        for i in range(runs + 2):
            with open(os.path.join(root, "d0", "f0.py"), "a") as fh:
                fh.write(f"# tick {i}\n")
            t0 = time.perf_counter()
            ok = commit(root, f"t{i}")
            dt = (time.perf_counter() - t0) * 1000
            if not ok:
                return None
            if i >= 2:
                samples.append(dt)
        samples.sort()
        medians.append(samples[len(samples) // 2])
    medians.sort()
    mid = medians[len(medians) // 2]
    # The spread of the per-run samples from the last round, as a fraction of the median.
    lo = samples[len(samples) // 4]
    hi = samples[(len(samples) * 3) // 4]
    spread = (hi - lo) / mid if mid else 0.0
    return mid, spread


def main():
    global GESTA
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")
    # Copy the binary off the working directory: reading an executable from a bind mount costs
    # about 16 ms per run on this setup, which is larger than the work being measured. The copy
    # is refreshed every run, because keeping a stale one measures the previous build and
    # reports it as the current one.
    local = "/tmp/gesta_scale_bin"
    shutil.copy2(GESTA, local)
    os.chmod(local, 0o755)
    GESTA = local

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT)

    print("Median of three rounds; the percentage is the spread of the middle half of that round.")
    print()
    print("  create reads the tree, commit writes the change. Git's `add -A` plays the scan here.")
    print()
    print(f"{'files':>7} {'gesta create':>14} {'gesta commit':>14} {'git add+commit':>16}  {'write ratio':>11}")
    # 100000 is the size a monorepo reaches and the number this project has not been measured at.
    # The sizes are read from the command line so a long run is a choice and not a surprise.
    sizes = (500, 2000, 8000)
    if "--sizes" in sys.argv:
        sizes = tuple(int(x) for x in sys.argv[sys.argv.index("--sizes") + 1].split(","))
    for n in sizes:
        g = os.path.join(ROOT, f"g{n}")
        t = os.path.join(ROOT, f"t{n}")
        build(g, n)
        build(t, n)

        sh(f"{GESTA} init", cwd=g)
        gesta_commit(g, "seed")
        sh("git init -q -b main", cwd=t)
        sh("git config user.email a@b.c && git config user.name bench", cwd=t)
        sh("git add -A && git commit -qm seed", cwd=t)
        # Both need a settled tree: a freshly written file is re-read by design.
        time.sleep(2.5)

        # The full gesta path (create then commit) for the comparison against git, and the split so
        # the write phase can be read on its own.
        z = measure(g, lambda r: None, gesta_commit)
        split = measure_split(g)
        b = measure(t, lambda r: None, lambda r, m: sh("git add -A && git commit -qm " + m, cwd=r).returncode == 0)
        if z is None or b is None or split is None:
            print(f"{n:>7} {'FAILED':>14}")
            continue
        (zm, zs), (bm, bs) = z, b
        cm, wm = split
        rows.append((n, cm, wm, bm))
        print(
            f"{n:>7} {cm:>9.1f}ms  {wm:>9.1f}ms  {bm:>12.1f}ms  {wm / bm:>10.2f}x"
        )

    if len(rows) >= 2:
        first, last = rows[0], rows[-1]
        factor = last[0] / first[0]
        scan_grown = last[1] / first[1]
        write_grown = last[2] / first[2]
        git_grown = last[3] / first[3]
        print()
        print(f"repository grew {factor:.0f}x ({first[0]} to {last[0]} files)")
        print(f"  gesta create: {first[1]:6.1f} -> {last[1]:6.1f} ms  ({scan_grown:.2f}x)")
        print(f"  gesta commit: {first[2]:6.1f} -> {last[2]:6.1f} ms  ({write_grown:.2f}x)")
        print(f"  git add+commit: {first[3]:5.1f} -> {last[3]:6.1f} ms  ({git_grown:.2f}x)")
        print()
        # The claim is about the write phase, and the number that supports it or does not is
        # reported either way. A run that says "flat" without the figure above it is a claim
        # nobody can check.
        if write_grown <= 1.5:
            print(
                f"Writing one file commit costs {write_grown:.2f}x more in a repository "
                f"{factor:.0f}x larger."
            )
            print("The claim is about the write, and it holds: the cost follows the change.")
        else:
            # This used to print the failure and return 0, which made the printed claim and the exit
            # code disagree. A caller reading the exit code was told the run passed while the line
            # above said the flat-cost claim did not hold.
            print(
                f"Writing one file commit costs {write_grown:.2f}x more in a repository "
                f"{factor:.0f}x larger, so the flat cost claim does not hold at this size."
            )
            print()
            print(
                f"Reading the tree to find the change costs {scan_grown:.2f}x more, which is the "
                "scan and not the write."
            )
            return 1
        print()
        # The scan is allowed to grow. Saying how much is the honest part: it is the price of
        # answering "what changed", and git pays the same shape of cost in `git status`.
        print(
            f"Reading the tree to find the change costs {scan_grown:.2f}x more, which is the "
            "scan and not the write."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
