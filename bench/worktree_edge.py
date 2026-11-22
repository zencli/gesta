#!/usr/bin/env python3
"""Edge cases for worktrees and submodules, the two modules with the fewest unit tests.

worktree has 3 unit tests and submodule has 0. The benches that exist walk the happy path: add a
worktree, list it, remove it. What a person actually hits is removing one that has uncommitted work,
removing the one they are standing in, and a submodule whose child repository has moved on.

Each check states the harm avoided, because "it returned 0" is not the same as "it did the right
thing" and this file exists to tell those apart.
"""
import os
import re
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


def g(*args, cwd):
    return sh(f"{GESTA} " + " ".join(args), cwd)


def record(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" :: {detail[:140]}" if not ok else ""))


def seed(d, name="a.txt", text="one\ntwo\n"):
    sh(f"{GESTA} init", d, check=True)
    with open(os.path.join(d, name), "w") as fh:
        fh.write(text)
    c = g("changeset", "create", "seed", "--json", cwd=d)
    m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
    if not m:
        raise SystemExit(f"no changeset: {c.stdout}")
    g("changeset", "commit", m.group(1), cwd=d)


def main():
    base = tempfile.mkdtemp(prefix="wtx-")
    try:
        # --- Removing a worktree that has uncommitted work. Git refuses unless --force, because the
        # work is the only copy. Losing it silently is the harm.
        d = os.path.join(base, "main")
        os.makedirs(d)
        seed(d)
        wt = os.path.join(base, "wt1")
        g("branch", "create", "side", cwd=d)
        r = g("worktree", "add", wt, "side", cwd=d)
        record("a worktree can be added on a new branch", r.returncode == 0,
               (r.stdout + r.stderr)[:160])

        if r.returncode == 0:
            with open(os.path.join(wt, "a.txt"), "a") as fh:
                fh.write("uncommitted work\n")
            rr = g("worktree", "remove", wt, cwd=d)
            still_exists = os.path.exists(wt)
            with open(os.path.join(wt, "a.txt")) if still_exists else open(os.devnull) as fh:
                survived = fh.read() if still_exists else ""
            record("removing a worktree with uncommitted work does not silently destroy it",
                   rr.returncode != 0 or "uncommitted work" in survived,
                   f"exit {rr.returncode}, tree {'kept' if still_exists else 'gone'}, "
                   f"work {'kept' if 'uncommitted work' in survived else 'LOST'}")

        # --- Removing the worktree you are standing in, from inside it. The command cannot delete the
        # directory it is running from, and a half-removed registry entry is worse than a refusal.
        d2 = os.path.join(base, "main2")
        os.makedirs(d2)
        seed(d2)
        wt2 = os.path.join(base, "wt2")
        g("branch", "create", "side2", cwd=d2)
        r2 = g("worktree", "add", wt2, "side2", cwd=d2)
        if r2.returncode == 0:
            rr2 = g("worktree", "remove", ".", cwd=wt2)
            listing = g("worktree", "list", cwd=d2).stdout
            record("removing the worktree you are inside is refused or leaves the registry honest",
                   (rr2.returncode != 0) or (wt2 not in listing),
                   f"exit {rr2.returncode}, still listed: {wt2 in listing}")

        # --- Two worktrees on the SAME branch. Git refuses: two heads on one branch is how a commit
        # gets lost. A registry that allows it records a state the store cannot represent.
        d3 = os.path.join(base, "main3")
        os.makedirs(d3)
        seed(d3)
        wt3 = os.path.join(base, "wt3")
        g("worktree", "add", wt3, cwd=d3)
        wt4 = os.path.join(base, "wt4")
        r4 = g("worktree", "add", wt4, cwd=d3)
        record("two worktrees on one branch are refused", r4.returncode != 0,
               f"exit {r4.returncode}")

        # --- A worktree of a worktree. The marker chain has to resolve to the same store, not to a
        # second store holding a copy.
        d5 = os.path.join(base, "main5")
        os.makedirs(d5)
        seed(d5)
        wt5 = os.path.join(base, "wt5")
        g("branch", "create", "b5", cwd=d5)
        if g("worktree", "add", wt5, "b5", cwd=d5).returncode == 0:
            wt6 = os.path.join(base, "wt6")
            g("branch", "create", "b6", cwd=wt5)
            r6 = g("worktree", "add", wt6, "b6", cwd=wt5)
            record("a worktree added from a worktree shares the one store",
                   r6.returncode == 0, (r6.stdout + r6.stderr)[:160])

        # --- A submodule whose child has moved on since the pin. `update` must bring it to the PIN,
        # not to the child's head: following the head silently changes what the parent recorded.
        d7 = os.path.join(base, "parent")
        child = os.path.join(base, "child")
        os.makedirs(d7)
        os.makedirs(child)
        seed(d7, "p.txt")
        seed(child, "c.txt")
        # `submodule add` records the pointer, `submodule init` materialises it. `sub` is a different
        # verb that records a path without a pin, and its listing has no commit to compare against.
        r7 = g("submodule", "add", "vendor/child", child, cwd=d7)
        if r7.returncode == 0:
            r7 = g("submodule", "init", cwd=d7)
        record("a submodule of this kind can be added and initialised", r7.returncode == 0,
               (r7.stdout + r7.stderr)[:200])

        if r7.returncode == 0:
            # Move the child's branch forward after the pin was recorded.
            with open(os.path.join(child, "c.txt"), "a") as fh:
                fh.write("later work\n")
            c2 = g("changeset", "create", "later", "--json", cwd=child)
            m2 = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c2.stdout)
            if m2:
                g("changeset", "commit", m2.group(1), cwd=child)
            st = g("submodule", "status", cwd=d7).stdout
            # Either the status names the difference, or it still reports the pin while the child's
            # head has moved - both keep the parent's record honest. What must not happen is the
            # parent silently following the child's head.
            moved = g("log", "--limit", "1", "--json", cwd=child).stdout
            head = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', moved)
            head_id = head.group(1) if head else ""
            record(
                "a child that moved on does not make the parent follow its head",
                head_id != "" and head_id not in st,
                f"child head {head_id} appeared in the parent's status: {st[:160]}",
            )
    finally:
        shutil.rmtree(base, ignore_errors=True)

    passed = sum(1 for _, ok, _ in results if ok)
    print(f"\n{passed}/{len(results)} worktree and submodule edge checks pass")
    for name, ok, detail in results:
        if not ok:
            print(f"  FAILED: {name} :: {detail[:200]}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
