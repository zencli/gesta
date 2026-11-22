#!/usr/bin/env python3
"""Does a git submodule survive a round trip through import and export?

git records a submodule as a tree entry with mode 160000 whose hash is a commit in ANOTHER repository.
There is no content to copy: the entry is a pointer, and it must be kept as one.

The failure this checks against is that import skips the entry, so the path vanishes from the tree and
the export cannot put it back. Nothing errors; the submodule is simply gone.
"""
import os
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


def git(*args, cwd="."):
    return sh("git " + " ".join(args), cwd)


def shlex_path(p):
    import shlex
    return shlex.quote(p)


def record(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" :: {detail[:160]}" if not ok else ""))


def main():
    if git("--version", cwd=os.getcwd()).returncode != 0:
        print("0/0 (no git)")
        return 0
    base = tempfile.mkdtemp(prefix="gl-")
    try:
        # A child git repository, so a real submodule can be added the way a person would.
        child = os.path.join(base, "child")
        os.makedirs(child)
        git("init -q .", cwd=child)
        open(os.path.join(child, "c.txt"), "w").write("child\n")
        git("add -A", cwd=child)
        git("-c user.email=a@b -c user.name=a commit -qm one", cwd=child)

        parent = os.path.join(base, "parent")
        os.makedirs(parent)
        git("init -q .", cwd=parent)
        open(os.path.join(parent, "p.txt"), "w").write("parent\n")
        git("add -A", cwd=parent)
        git("-c user.email=a@b -c user.name=a commit -qm one", cwd=parent)
        # `-c protocol.file.allow=always` because newer git refuses file-protocol submodules by
        # default, and a test is exactly the case where the source is a local path.
        # No quoting: this runs through a shell, and a quoted temp path arrives with the quotes
        # intact, which git reads as part of the repository name. mkdtemp does not put spaces here.
        r = git('-c protocol.file.allow=always submodule add ' + child + " sub", cwd=parent)
        if r.returncode != 0:
            print("  (submodule add failed: " + (r.stdout + r.stderr).strip()[:200] + ")")
        git("add -A", cwd=parent)
        git("-c user.email=a@b -c user.name=a commit -qm add-sub", cwd=parent)
        head = git("rev-parse HEAD", cwd=parent).stdout.strip()
        ls = git("ls-tree -r HEAD", cwd=parent).stdout
        has_gitlink = "160000" in ls
        record("the fixture really holds a gitlink", has_gitlink, ls[:200])

        if not has_gitlink:
            print("cannot test without a gitlink fixture")
        else:
            # Import into this tool, then look at what the tree holds.
            mine = os.path.join(base, "mine")
            os.makedirs(mine)
            g("init", cwd=mine)
            imp = g("git", "import", parent, cwd=mine)
            record("the repository with a submodule imports", imp.returncode == 0,
                   (imp.stdout + imp.stderr)[:240])

            # The pointer is recorded as a subrepository, not as a tree entry: this store's tree holds
            # blobs by content hash, and a gitlink names a commit in a child repository that this
            # store has no object for. So the check is that the pointer was recorded, and that the
            # export put it back - which is the row below.
            subs = g("sub", "list", "--json", cwd=mine).stdout
            record("the submodule pointer is recorded on import",
                   '"sub"' in subs or '"/sub"' in subs or '"path":"sub"' in subs.replace(" ", ""),
                   subs[:240])

            # Export back to git and check the gitlink is still a gitlink, not a file and not absent.
            out = os.path.join(base, "back")
            os.makedirs(out)
            git("init -q .", cwd=out)
            exp = g("git", "export", out, cwd=mine)
            record("the repository exports back to git", exp.returncode == 0,
                   (exp.stdout + exp.stderr)[:240])
            # Not HEAD: a freshly initialised repository has no HEAD commit, so `ls-tree HEAD` is
            # empty and the check fails on a repository that exported correctly. The branch the
            # export wrote is what has to be read.
            back = ""
            # Quote the format: unquoted, the shell reads `(` as syntax and git exits 2 with an
            # empty list, which looked exactly like "nothing was exported".
            for br in git(
                "for-each-ref '--format=%(refname:short)' refs/heads", cwd=out
            ).stdout.split():
                back = git(f"ls-tree -r {br}", cwd=out).stdout
                if back:
                    break
            if not back:
                back = git("ls-tree -r HEAD", cwd=out).stdout
            record("the gitlink is still a gitlink after a round trip",
                   "160000" in back and "sub" in back,
                   back[:260]
                   or f"export exit {exp.returncode}: {(exp.stdout + exp.stderr).strip()[:200]}")
    finally:
        shutil.rmtree(base, ignore_errors=True)

    passed = sum(1 for _, ok, _ in results if ok)
    print(f"\n{passed}/{len(results)} gitlink checks pass")
    for name, ok, detail in results:
        if not ok:
            print(f"  FAILED: {name} :: {detail[:220]}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
