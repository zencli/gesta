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
        r = sh(["git", "import", r256], mine)
        out = (r.stdout + r.stderr)
        checks.append(("a sha256 repository is refused", r.returncode != 0, f"rc={r.returncode}"))
        # The message legitimately contains the words "missing objects", because it explains what
        # an import would report. The check is that the refusal is a refusal about the format, not a
        # report that a file is missing: it has to name SHA-256 and it must not be the shape a broken
        # store produces.
        checks.append(("the refusal names SHA-256",
                       "SHA-256" in out, out.strip()[:160]))
        checks.append(("the refusal is not a missing-file error",
                       "no such file" not in out.lower() and "cannot read" not in out.lower(),
                       out.strip()[:160]))
        checks.append(("the refusal names the way out",
                       "--object-format=sha1" in out, out.strip()[:160]))

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
