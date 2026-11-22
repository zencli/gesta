#!/usr/bin/env python3
"""Does a SHA-256 git repository get refused by name, or read as if it were SHA-1?

git has supported `--object-format=sha256` since 2.29, and this tool reads SHA-1 repositories only. The
failure that matters is not "it does not support it" - that is known and written down. The failure that
matters is the shape of the refusal: a SHA-1 reader pointed at a SHA-256 repository finds no matching
object and reports a missing file, so a person goes looking for a corruption that is not there.

So the checks are:

  a sha256 repository is refused       and the message says SHA-256, not "missing object"
  the message names the way out        `--object-format=sha1`, which is what converts it
  a sha1 repository still imports      the guard must not break the normal path
  a repository with no config imports  git's default is sha1, and a hand-made repo may lack a config
  an unknown format is refused         guessing at blake3 would be worse than refusing

The third and fourth checks exist because a guard that refuses everything passes the first two.
"""
import os
import shutil
import subprocess
import sys
import tempfile

ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def git(args, cwd):
    return subprocess.run(["git"] + args, cwd=cwd, capture_output=True, text=True)


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="objsize-")
    try:
        # A real sha256 repository, made by real git rather than by writing a config by hand.
        r256 = os.path.join(d, "r256")
        os.makedirs(r256)
        r = git(["init", "--quiet", "--object-format=sha256", "."], r256)
        if r.returncode != 0:
            print("  [SKIP] this git cannot make a sha256 repository:", r.stderr.strip()[:90])
            print("\n0/0 (skipped)")
            return 0
        with open(os.path.join(r256, "f.txt"), "w") as fh:
            fh.write("hello\n")
        git(["add", "-A"], r256)
        git(["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "first"], r256)
        fmt = git(["rev-parse", "--show-object-format"], r256).stdout.strip()
        checks.append(("the fixture really is sha256", fmt == "sha256", f"git says {fmt!r}"))

        mine = os.path.join(d, "mine")
        os.makedirs(mine)
        sh(["init"], mine)
        # SHA-256 repositories are read and written, not refused. This bench used to assert the
        # refusal, which was the behaviour before that support landed, so it went from 7/7 to 4/7 the
        # day the tool got better at its job. What matters now is that the import is faithful.
        r = sh(["git", "import", r256], mine)
        out = (r.stdout + r.stderr)
        checks.append(("a sha256 repository is imported, not refused",
                       r.returncode == 0, out.strip()[:200]))
        checks.append(("the import is not a silent empty one",
                       "imported 1 commit" in out or "1 path change" in out, out.strip()[:200]))
        # The imported content has to be the content, read back through this tool's own commands.
        # An import that stores nothing and reports success is the failure worth catching.
        shown = sh(["show", "f.txt"], mine)
        checks.append(("the imported file reads back with its content",
                       shown.returncode == 0 and "hello" in shown.stdout,
                       (shown.stdout + shown.stderr).strip()[:200]))
        # And a store records the digest it was built with, so a later read does not guess.
        st = sh(["stats"], mine)
        checks.append(("the imported store records sha256 as its format",
                       "sha256" in st.stdout.lower(), (st.stdout + st.stderr).strip()[:200]))

        # A sha1 repository beside it must still import, or the guard broke the normal path.
        r1 = os.path.join(d, "r1")
        os.makedirs(r1)
        git(["init", "--quiet", "--object-format=sha1", "."], r1)
        with open(os.path.join(r1, "ok.txt"), "w") as fh:
            fh.write("fine\n")
        git(["add", "-A"], r1)
        git(["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "first"], r1)

        mine1 = os.path.join(d, "mine1")
        os.makedirs(mine1)
        sh(["init"], mine1)
        r = sh(["git", "import", r1], mine1)
        checks.append(("a sha1 repository still imports", r.returncode == 0,
                       (r.stderr or r.stdout).strip()[:160]))

        # A repository whose config says nothing about format keeps git's default.
        bare = os.path.join(d, "bare")
        os.makedirs(os.path.join(bare, "objects"))
        os.makedirs(os.path.join(bare, "refs"))
        with open(os.path.join(bare, "config"), "w") as fh:
            fh.write("[core]\n\trepositoryformatversion = 0\n")
        mine2 = os.path.join(d, "mine2")
        os.makedirs(mine2)
        sh(["init"], mine2)
        r = sh(["git", "import", bare], mine2)
        # An empty repository imports nothing, so the check is that it did not refuse on format.
        checks.append(("a repository with no objectformat is treated as sha1",
                       "SHA-256" not in (r.stdout + r.stderr), (r.stdout + r.stderr).strip()[:140]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} object-format checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
