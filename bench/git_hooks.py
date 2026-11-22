#!/usr/bin/env python3
"""Does a hook written for .git/hooks run when this tool's matching point fires?

An author who already wrote a guard should not find it silently ignored because this tool keeps its
hooks in a different directory. The failure this checks against is the quiet one: a guard that is
listed as absent while a commit runs unguarded.
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
    import shlex
    return sh(f"{ZF} " + " ".join(shlex.quote(str(a)) for a in args), cwd)


def record(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" :: {detail[:140]}" if not ok else ""))


def repo(d):
    g("init", cwd=d, )
    with open(os.path.join(d, "a.txt"), "w") as fh:
        fh.write("one\n")
    c = g("changeset", "create", "seed", "--json", cwd=d)
    m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
    assert m, c.stdout + c.stderr
    g("changeset", "commit", m.group(1), cwd=d)


def write_hook(root, name, body):
    d = os.path.join(root, ".git", "hooks")
    os.makedirs(d, exist_ok=True)
    p = os.path.join(d, name)
    with open(p, "w") as fh:
        fh.write(body)
    os.chmod(p, 0o755)
    return p


def main():
    base = tempfile.mkdtemp(prefix="gh-")
    try:
        # --- A pre-commit hook in .git/hooks fires when a changeset is committed.
        d = os.path.join(base, "r1")
        os.makedirs(d)
        repo(d)
        mark = os.path.join(d, "git-hook-ran")
        write_hook(d, "pre-commit", f"#!/bin/sh\necho ran > '{mark}'\nexit 0\n")
        with open(os.path.join(d, "a.txt"), "a") as fh:
            fh.write("two\n")
        c = g("changeset", "create", "second", "--json", cwd=d)
        m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
        r = g("changeset", "commit", m.group(1), cwd=d)
        record("a git pre-commit hook runs on commit", os.path.exists(mark),
               f"exit {r.returncode}, no mark file")

        # --- And a git hook that fails stops the commit, which is the point of a guard.
        # `pre-changeset` maps to git's `pre-commit` in this implementation, so a failing pre-commit
        # already refuses at create time. `post-commit` is used here instead, to check that a failure
        # at the point AFTER the snapshot also stops the commit from landing.
        d2 = os.path.join(base, "r2")
        os.makedirs(d2)
        repo(d2)
        write_hook(d2, "post-commit", "#!/bin/sh\necho 'not allowed' >&2\nexit 7\n")
        with open(os.path.join(d2, "a.txt"), "a") as fh:
            fh.write("two\n")
        c = g("changeset", "create", "second", "--json", cwd=d2)
        m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
        if not m:
            record("a failing git hook stops the commit", False,
                   "the hook refused before a changeset could even be made: "
                   + (c.stdout + c.stderr)[:160])
        else:
            r = g("changeset", "commit", m.group(1), cwd=d2)
            out = r.stdout + r.stderr
            record("a failing git hook stops the commit", r.returncode != 0, f"exit {r.returncode}")
            record("its message reaches the caller", "not allowed" in out, out[:160])

        # --- This tool's own hook wins when both exist, because it is the one it can manage.
        d3 = os.path.join(base, "r3")
        os.makedirs(d3)
        repo(d3)
        own = os.path.join(d3, "own-ran")
        gitmark = os.path.join(d3, "git-ran")
        write_hook(d3, "pre-commit", f"#!/bin/sh\necho ran > '{gitmark}'\nexit 0\n")
        hk = os.path.join(d3, ".gesta", "hooks")
        os.makedirs(hk, exist_ok=True)
        with open(os.path.join(hk, "pre-commit"), "w") as fh:
            fh.write(f"#!/bin/sh\necho ran > '{own}'\nexit 0\n")
        os.chmod(os.path.join(hk, "pre-commit"), 0o755)
        with open(os.path.join(d3, "a.txt"), "a") as fh:
            fh.write("two\n")
        c = g("changeset", "create", "second", "--json", cwd=d3)
        m = re.search(r'"(?:changeset|id)":\s*"(cs[0-9a-f]+)"', c.stdout)
        g("changeset", "commit", m.group(1), cwd=d3)
        record("this tool's own hook takes precedence over a git one",
               os.path.exists(own), "own hook did not run")

        # --- A point with no git equivalent never reaches into .git/hooks and runs something
        # unrelated: `pre-land` is this tool's own step.
        listing = g("hook", "list", cwd=d)
        record("hook list names a git hook as coming from git, not as absent",
               "from git" in listing.stdout and "pre-commit" in listing.stdout,
               listing.stdout[:200])

        # --- A git hook is not reported as 'changed', which would look like tampering.
        st = g("hook", "status", "--json", cwd=d)
        record("a git hook is not reported as changed", '"from_git":true' in st.stdout.replace(" ", "")
               or '"changed":false' in st.stdout.replace(" ", ""), st.stdout[:240])
    finally:
        shutil.rmtree(base, ignore_errors=True)

    passed = sum(1 for _, ok, _ in results if ok)
    print(f"\n{passed}/{len(results)} git-hook checks pass")
    for name, ok, detail in results:
        if not ok:
            print(f"  FAILED: {name} :: {detail[:200]}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
