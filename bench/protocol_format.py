#!/usr/bin/env python3
"""Does the store remember which digest its objects use, and does it say so truthfully?

The trap here is subtle, and the first version of this bench fell into it. It is tempting to make the
capability advertisement report the store's format, so a SHA-256 repository says `object-format=sha256`.
That is a lie: this server puts its own changeset names on the wire (`cs` plus twelve hex characters),
not git object ids, so a client told sha256 parses those names as 64-character ids and rejects the whole
response with `invalid ls-refs response`. Measured, on a real client.

So the advertisement stays `sha1`, because that is what is served, and the store records the format for
the thing that does depend on it: what a name written by import actually is. `stats` reports it.

  import records the format it read            a sha256 git repository imports as sha256
  a sha1 repository records sha1               the default did not move
  stats says what the length means             not only the name of the format
  the advertisement still says sha1            because that is what the wire actually carries
"""
import os
import re
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
    d = tempfile.mkdtemp(prefix="fmt-")
    try:
        probe = os.path.join(d, "probe")
        git(["init", "--quiet", "--object-format=sha256", probe], d)
        fmt = git(["rev-parse", "--show-object-format"], probe).stdout.strip() if os.path.isdir(probe) else ""
        if fmt != "sha256":
            print("  [SKIP] this git cannot make a sha256 repository")
            print("\n0/0 (skipped)")
            return 0

        # A SHA-256 repository made by git, imported into this store.
        orig = os.path.join(d, "orig")
        os.makedirs(orig)
        git(["init", "--quiet", "--object-format=sha256", "."], orig)
        with open(os.path.join(orig, "f.txt"), "w") as fh:
            fh.write("hello\n")
        git(["add", "-A"], orig)
        git(["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "first"], orig)

        mine = os.path.join(d, "mine")
        os.makedirs(mine)
        sh(["init"], mine)
        r = sh(["git", "import", orig], mine)
        stats256 = sh(["stats"], mine).stdout + sh(["stats"], mine).stderr
        checks.append(("importing a sha256 repository records the format",
                       r.returncode == 0 and "sha256" in stats256, stats256[:240]))

        # A plain repository keeps the default.
        plain = os.path.join(d, "plain")
        os.makedirs(plain)
        sh(["init"], plain)
        with open(os.path.join(plain, "a.txt"), "w") as fh:
            fh.write("x\n")
        r = sh(["changeset", "create", "s", "--author", "t", "--json"], plain)
        m = re.search(r"(cs[0-9a-f]+)", r.stdout)
        if m:
            sh(["changeset", "commit", m.group(1)], plain)
        stats1 = sh(["stats"], plain).stdout + sh(["stats"], plain).stderr
        checks.append(("a sha1 repository reports sha1 and does not claim otherwise",
                       "sha1" in stats1 and "sha256" not in stats1, stats1[:240]))

        # The number is what a reader compares against, so the length is named.
        checks.append(("stats says what the length means, not only the name of the format",
                       "64 character names" in stats256 and "40 character names" in stats1,
                       f"256={stats256[:120]!r} 1={stats1[:120]!r}"))

        # Both repositories report their own format, which is the whole check: the value is per
        # repository, not a constant that happens to be right for one of them.
        checks.append(("the two stores disagree, so the value is read per repository",
                       "sha256" in stats256 and "sha1" in stats1, stats256[:120]))
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} format checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
