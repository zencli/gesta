#!/usr/bin/env python3
"""Does the store survive many writers, and what does serialisation cost?

SQLite takes one writer at a time. That is a design choice, not a defect, but it has a limit: past
some number of concurrent writers the queue either drains or it does not. This measures where that
limit is, in writers and in wall time, rather than asserting a number nobody checked.

Two things are measured apart:

- **Correctness first.** Every writer must land exactly one commit, `fsck` must be clean, and no
  commit may be lost. A fast run that loses a commit is not a result.
- **Cost second.** The wall time to drain N writers, and how it grows from N to 4N. A queue that
  serialises shows linear growth in total time and flat per-commit cost; a queue that breaks shows
  errors or a sublinear time that means writers were dropped.

Run: python3 bench/many_writers.py [--writers 20,50,100]
"""

import os
import shutil
import statistics
import subprocess
import sys
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = "/tmp/gesta_many_writers"


def run(cmd, cwd=None, timeout=600):
    return subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def one_writer(root, i, started):
    """Create and commit one changeset. Returns (ok, milliseconds, error)."""
    path = os.path.join(root, f"w{i}.txt")
    with open(path, "w") as fh:
        fh.write(f"writer {i}\n")

    t0 = time.perf_counter()
    # Scoped to this writer's own file. Without `--path` each writer snapshots the whole tree and
    # records the other writers' files under its own name, which is the bug this run found.
    created = run(
        f'{ZF} changeset create "w{i}" --author p{i} --path w{i}.txt --json', cwd=root
    )
    cid = ""
    if created.returncode == 0:
        import json
        try:
            cid = json.loads(created.stdout).get("changeset") or ""
        except Exception:
            cid = ""
    if not cid:
        return False, (time.perf_counter() - t0) * 1000, (created.stdout + created.stderr)[-200:]

    committed = run(f"{ZF} changeset commit {cid}", cwd=root)
    ms = (time.perf_counter() - t0) * 1000
    if committed.returncode != 0:
        return False, ms, (committed.stdout + committed.stderr)[-200:]
    return True, ms, ""


def measure(n):
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT, exist_ok=True)
    r = run(f"{ZF} init", cwd=ROOT)
    if r.returncode != 0:
        print(f"init failed: {r.stdout}{r.stderr}")
        return None

    # A base commit so the writers start from a repository that already has history.
    with open(os.path.join(ROOT, "base.txt"), "w") as fh:
        fh.write("base\n")
    run(f'{ZF} changeset create "base" --author b', cwd=ROOT)
    out = run(f"{ZF} changeset list --json", cwd=ROOT).stdout
    import json
    try:
        items = json.loads(out)
        ids = [x.get("id") for x in (items if isinstance(items, list) else items.get("changesets", []))]
    except Exception:
        ids = []
    if ids:
        run(f"{ZF} changeset commit {ids[0]}", cwd=ROOT)

    # All writers start together. A staggered start measures the stagger.
    procs = []
    t0 = time.perf_counter()
    for i in range(n):
        procs.append(
            subprocess.Popen(
                [sys.executable, "-c",
                 f"import sys; sys.path.insert(0, {_HERE!r});"
                 f"from many_writers import one_writer;"
                 f"ok, ms, err = one_writer({ROOT!r}, {i}, 0);"
                 f"print('OK' if ok else 'FAIL', ms, err.replace(chr(10), ' ')[:120])"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
        )
    results = []
    for p in procs:
        out, err = p.communicate(timeout=900)
        results.append(out.strip() or err.strip())
    wall = (time.perf_counter() - t0) * 1000

    ok = sum(1 for r in results if r.startswith("OK"))
    per = []
    for r in results:
        parts = r.split()
        if len(parts) >= 2 and parts[0] == "OK":
            try:
                per.append(float(parts[1]))
            except ValueError:
                pass

    log = run(f"{ZF} log --limit 500 --oneline", cwd=ROOT).stdout
    total = log.count("cs")
    fsck = run(f"{ZF} fsck", cwd=ROOT)
    on_disk = len([f for f in os.listdir(ROOT) if f.endswith(".txt")])

    return {
        "n": n,
        "ok": ok,
        "wall_ms": wall,
        "median_ms": statistics.median(per) if per else None,
        "p95_ms": sorted(per)[int(len(per) * 0.95)] if len(per) > 4 else None,
        "changesets": total,
        "fsck_ok": fsck.returncode == 0 and "consistent" in fsck.stdout,
        "files": on_disk,
        "failures": [r for r in results if not r.startswith("OK")][:3],
    }


def main():
    sizes = [20, 50]
    if "--writers" in sys.argv:
        sizes = [int(x) for x in sys.argv[sys.argv.index("--writers") + 1].split(",")]

    print(f"{'writers':>8} {'ok':>4} {'wall ms':>9} {'median':>8} {'p95':>8} {'commits':>8} {'fsck':>5} {'files':>6}")
    print("-" * 66)
    rows = []
    for n in sizes:
        r = measure(n)
        if r is None:
            return 1
        rows.append(r)
        med = f"{r['median_ms']:.0f}" if r["median_ms"] else "-"
        p95 = f"{r['p95_ms']:.0f}" if r["p95_ms"] else "-"
        print(
            f"{r['n']:>8} {r['ok']:>4} {r['wall_ms']:>9.0f} {med:>8} {p95:>8} "
            f"{r['changesets']:>8} {'ok' if r['fsck_ok'] else 'BAD':>5} {r['files']:>6}"
        )
        if r["failures"]:
            for f in r["failures"]:
                print(f"    failure: {f}")

    print()
    failed = [r for r in rows if r["ok"] != r["n"] or not r["fsck_ok"] or r["changesets"] < r["n"] + 1]
    if failed:
        print(f"NOT SOUND: {len(failed)} size(s) lost a commit or broke the store")
        for r in failed:
            print(
                f"  {r['n']} writers: {r['ok']} reported ok, {r['changesets']} changesets "
                f"(expected {r['n'] + 1}), fsck {'clean' if r['fsck_ok'] else 'FAILED'}"
            )
        return 1

    print("every writer landed a commit and the store is consistent")
    if len(rows) >= 2:
        a, b = rows[0], rows[-1]
        growth = b["n"] / a["n"]
        cost = b["wall_ms"] / a["wall_ms"] if a["wall_ms"] else 0
        # Reported, not asserted. Measured on this host at a fixed input pair (20 -> 50 writers), three
        # consecutive runs grew the wall time 4.62x, 3.06x and 1.73x for the same 2.5x growth in
        # writers - a spread of 2.7x inside one input size. No estimator survives that, and the earlier
        # threshold (`cost <= growth * 1.6`) therefore failed on whichever run drew the slow sample:
        # five times in one session, always passing again on the next run.
        #
        # The claim that matters - every writer committed and nothing was lost - is the count above,
        # which cannot be moved by the host. A duration here can only ever report the machine.
        print(
            f"for reference only: writers grew {growth:.1f}x ({a['n']} -> {b['n']}); "
            f"wall time grew {cost:.2f}x ({a['wall_ms']:.0f} -> {b['wall_ms']:.0f} ms)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
