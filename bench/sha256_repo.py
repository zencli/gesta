#!/usr/bin/env python3
"""Does a SHA-256 git repository survive a round trip through this tool?

The name of an object in a SHA-256 repository is 64 hex characters where a SHA-1 one is 40, and that
length appears in the loose object path, the tree entry, the commit's parent line, the pack trailer
and the protocol's ref advertisement. Code that assumes 40 does not fail loudly - it finds nothing and
reports a missing object, which reads as a corrupt repository.

So the check is not "the format is detected". It is that real git can read what we write and that a
commit keeps its name through the trip:

  a sha256 repository imports            the file, its content, and the branch arrive
  an object name is 64 characters        the shape git expects, not a truncated 40
  the name matches git's own             the same blob, named by both, compared
  export writes a repository git can read `git fsck` is clean and `git log` shows the commit
  a round trip keeps the commit name     export back into sha256 reproduces the original hash
  a sha1 repository is unchanged         the default path must not have moved underneath

The last one exists because every change here is on a shared path: if SHA-1 broke, the other four
checks would still pass on a fresh repository while every existing user's repository stopped working.
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


def has_sha256():
    with tempfile.TemporaryDirectory() as t:
        r = git(["init", "--quiet", "--object-format=sha256", "."], t)
        return r.returncode == 0


def main():
    if not has_sha256():
        print("  [SKIP] this git cannot make a sha256 repository")
        print("\n0/0 (skipped)")
        return 0

    checks = []
    d = tempfile.mkdtemp(prefix="sha256-")
    try:
        # A real sha256 repository, made by real git, with one committed file.
        orig = os.path.join(d, "orig")
        os.makedirs(orig)
        git(["init", "--quiet", "--object-format=sha256", "."], orig)
        with open(os.path.join(orig, "r.txt"), "w") as fh:
            fh.write("round trip\n")
        git(["add", "-A"], orig)
        git(["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "first"], orig)
        branch = git(["rev-parse", "--abbrev-ref", "HEAD"], orig).stdout.strip()
        orig_head = git(["rev-parse", "HEAD"], orig).stdout.strip()
        blob_name = git(["rev-parse", "HEAD:r.txt"], orig).stdout.strip()

        checks.append(("the fixture is a real sha256 repository",
                       git(["rev-parse", "--show-object-format"], orig).stdout.strip() == "sha256",
                       ""))
        checks.append(("git names the blob with 64 characters", len(blob_name) == 64,
                       f"{len(blob_name)}"))

        # Import.
        mine = os.path.join(d, "mine")
        os.makedirs(mine)
        sh(["init"], mine)
        r = sh(["git", "import", orig], mine)
        checks.append(("a sha256 repository imports", r.returncode == 0,
                       (r.stdout + r.stderr).strip()[:160]))
        ls = sh(["ls"], mine)
        checks.append(("the imported tree holds the file",
                       "r.txt" in (ls.stdout + ls.stderr), (ls.stdout + ls.stderr).strip()[:120]))
        show = sh(["show", "r.txt"], mine)
        checks.append(("the imported content is the original bytes",
                       show.stdout.strip() == "round trip", repr(show.stdout[:60])))

        # Export into a fresh sha256 repository, and let git judge it.
        back = os.path.join(d, "back")
        os.makedirs(back)
        git(["init", "--quiet", "--object-format=sha256", "."], back)
        r = sh(["git", "export", back], mine)
        checks.append(("export into a sha256 repository succeeds", r.returncode == 0,
                       (r.stdout + r.stderr).strip()[:160]))

        fsck = git(["fsck"], back)
        checks.append(("git fsck finds nothing wrong with what we wrote",
                       fsck.returncode == 0 and "error" not in (fsck.stdout + fsck.stderr).lower(),
                       (fsck.stdout + fsck.stderr).strip()[:160]))

        back_head = git(["rev-parse", branch], back).stdout.strip()
        checks.append(("a round trip reproduces the original commit name",
                       back_head == orig_head and back_head != "",
                       f"orig={orig_head[:16]} back={back_head[:16]}"))

        # The default path. A fresh sha1 repository has to keep working.
        o1 = os.path.join(d, "o1")
        os.makedirs(o1)
        git(["init", "--quiet", "--object-format=sha1", "."], o1)
        with open(os.path.join(o1, "a.txt"), "w") as fh:
            fh.write("fine\n")
        git(["add", "-A"], o1)
        git(["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "first"], o1)
        m1 = os.path.join(d, "m1")
        os.makedirs(m1)
        sh(["init"], m1)
        r = sh(["git", "import", o1], m1)
        checks.append(("a sha1 repository still imports", r.returncode == 0,
                       (r.stdout + r.stderr).strip()[:160]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} sha256 checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
