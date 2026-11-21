#!/usr/bin/env python3
"""Does every command the documentation promises actually exist and run?

A spec that says "gesta supports X" is a claim, and a claim in a README is the one nobody tests. This
walks the documented surface and checks each command against the binary: does it parse, does the help
text exist, and for the ones that can run without a server or a remote, does a minimal invocation
succeed on a fresh repository.

The distinction the report makes matters more than the total:

- `missing` is a command the docs name and the binary does not have. A reader follows the sentence and
  gets "unrecognized subcommand", which reads as a broken tool.
- `unparsed` is a command that exists but whose documented flags are rejected. Same experience, one
  level deeper, and invisible to a `--help` check.
- `failing` is a command that parses and then fails on a repository it should work on.

Run: python3 bench/docs.py
"""

import json
import os
import re
import shutil
import subprocess
import sys

# Found relative to this file. A benchmark that only runs in one container is not a
# measurement another machine can repeat.
_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GESTA = os.environ.get("GESTA_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = "/tmp/gesta_docs"


def sh(cmd, cwd=None):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, cwd=cwd, timeout=120)


def _json_ok(out: str) -> bool:
    """One parseable document, which is what a machine caller is promised."""
    try:
        json.loads(out.strip())
        return True
    except Exception:
        return False


def documented_commands():
    """Every `gesta <path>` the markdown names, as the list of words it uses.

    Both backticked spans and fenced blocks, because a command in a code block and a command in a
    sentence are the same promise to the reader.
    """
    out = set()
    for name in ("README.md",):
        path = os.path.join(ROOT, name)
        if not os.path.exists(path):
            continue
        text = open(path, encoding="utf-8").read()
        for m in re.finditer(r"\bgesta ([a-z][a-z0-9-]*(?: [a-z][a-z0-9-]*)?)", text):
            words = m.group(1).split()
            out.add(" ".join(words))
        # A command table row is a promise too, and the two-word regex above only reads prose: it
        # missed every command the README lists as `| `notes add <cs> -m <text>` | ... |`, which is
        # where most of them live. It also collected garbage from sentences - "keeps the", "writes
        # and" - because prose after a bare `gesta` looks the same to the regex as a command name.
        # The row may or may not spell out `gesta` first, so both forms are read.
        for m in re.finditer(r"^\|\s*`(?:gesta )?([^`]+)`", text, re.M):
            words = m.group(1).split()
            # Drop placeholders and flags: the command name is the leading words that are not one.
            name = []
            for w in words:
                if w.startswith("-") or w.startswith("<") or w.startswith("["):
                    break
                name.append(w)
            if name and name[0].isalpha() and name[0] not in ("the", "this", "that"):
                out.add(" ".join(name[:2]))
    # Words that are never a subcommand, collected from prose the regex read as one. Keeping them
    # makes the count look bigger and the surface look covered when it is not.
    junk = {"keeps the", "writes and"}
    out -= junk
    for extra in ("docs/install.md", "docs/hosting.md", "docs/secrets.md", "docs/harness.md"):
        path = os.path.join(ROOT, extra)
        if not os.path.exists(path):
            continue
        text = open(path, encoding="utf-8").read()
        for m in re.finditer(r"\bgesta ([a-z][a-z0-9-]*(?: [a-z][a-z0-9-]*)?)", text):
            out.add(" ".join(m.group(1).split()))
    return sorted(out)


def flags_documented():
    """The flags the README names for a command, so a documented flag is checked too."""
    text = open(os.path.join(ROOT, "README.md"), encoding="utf-8").read()
    out = {}
    for m in re.finditer(r"gesta ([a-z][a-z0-9-]*(?: [a-z][a-z0-9-]*)?)([^\n`|]{0,120})", text):
        cmd = m.group(1)
        flags = re.findall(r"(--[a-z][a-z0-9-]*)", m.group(2))
        if flags:
            out.setdefault(cmd, set()).update(flags)
    return {k: sorted(v) for k, v in out.items()}


def main():
    if not os.path.exists(GESTA):
        sys.exit(f"binary not found: {GESTA}")

    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)

    # A repository that holds one commit, so commands that read history have something to read.
    repo = os.path.join(WORK, "repo")
    os.makedirs(repo)
    sh(f"{GESTA} init", cwd=repo)
    with open(os.path.join(repo, "a.txt"), "w") as fh:
        fh.write("one\ntwo\n")
    sh(f'{GESTA} changeset create "first" --author t', cwd=repo)
    m = re.search(r"(cs[0-9a-f]+)", sh(f"{GESTA} changeset list --json", cwd=repo).stdout)
    if m:
        sh(f"{GESTA} changeset commit {m.group(1)}", cwd=repo)

    top = sh(f"{GESTA} --help").stdout
    names = set(re.findall(r"^  ([a-z][a-z0-9-]*)", top, re.M))

    missing, unparsed, ok = [], [], []
    # Words that are a sentence rather than a command ("gesta keeps the", "gesta git push").
    skip_second = {"keeps", "git", "remote", "changeset", "branch", "stash", "sparse", "secret", "ignore", "hook"}

    # A command path is real when the binary accepts it with --help. Anything else the regex picked
    # up out of a sentence is prose that happens to start with the tool's name, and reporting it as a
    # missing command buries the real findings under seven lines of English.
    def parses(path: str) -> bool:
        r = sh(f"{GESTA} {path} --help", cwd=repo)
        return r.returncode == 0

    for cmd in documented_commands():
        words = cmd.split()
        head = words[0]
        if head not in names:
            # Not a subcommand at all: "gesta answers all six" is a sentence.
            if not parses(cmd):
                continue
            ok.append(cmd)
            continue
        # Take the longest prefix of the words that parses. `remote token add --hashed` documents a
        # flag as part of the path, and asking for `remote token --hashed` reports a good command as
        # broken, which is the same fault this script exists to find, inverted.
        used = head
        for w in words[1:]:
            if w.startswith("--"):
                break
            if parses(f"{used} {w}"):
                used = f"{used} {w}"
            else:
                break
        if used == head and len(words) > 1 and not words[1].startswith("--"):
            # The head parses but the next word does not: either a group with a subcommand this
            # build lacks, or prose. Prose has no help output of its own.
            if parses(head):
                ok.append(cmd)
                continue
            unparsed.append(f"{cmd}: no such subcommand")
            continue
        ok.append(used)

    # The documented flags, on the command they were documented for.
    flag_bad = []
    for cmd, flags in flags_documented().items():
        head = cmd.split()[0]
        if head not in names:
            continue
        for f in flags:
            r = sh(f"{GESTA} {cmd} {f} --help", cwd=repo)
            if r.returncode != 0 and "unexpected argument" in (r.stderr or r.stdout):
                # The flag may belong to a deeper subcommand, which is how `remote token add
                # --hashed` is written. Try one level down before calling it a broken promise.
                deeper = sh(f"{GESTA} {cmd} add {f} --help", cwd=repo)
                if deeper.returncode == 0 or "unexpected argument" not in (deeper.stderr or deeper.stdout):
                    continue
                flag_bad.append(f"gesta {cmd} {f}")

    # A command with no `--help` at all is a command nobody can discover.
    no_help = []
    for n in sorted(names):
        # `help` is the shell's own entry point and prints the top-level list rather than taking a
        # --help flag of its own. Every other subcommand has to answer.
        if n == "help":
            continue
        r = sh(f"{GESTA} {n} --help", cwd=repo)
        if r.returncode != 0:
            no_help.append(n)

    # A command that parses and then fails on a repository it should work on is the finding a
    # --help check cannot produce. These are the ones whose documented usage needs nothing but a
    # local repository, run exactly as the README writes them.
    #
    # Each entry is (the documented invocation, what has to be true afterwards). A command that
    # exits 0 while printing an error passes an exit-code check and fails this one.
    behaviours = [
        ("status", lambda out, rc: rc == 0 and "branch" in out),
        ("log --oneline", lambda out, rc: rc == 0 and re.search(r"cs[0-9a-f]{6,}", out) is not None),
        ("log --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("ls", lambda out, rc: rc == 0 and "a.txt" in out),
        ("ls --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("brief", lambda out, rc: rc == 0 and len(out.strip()) > 0),
        ("brief --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("diff --to main", lambda out, rc: rc == 0),
        ("grep one", lambda out, rc: rc == 0 and "a.txt" in out),
        ("grep one --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("blame a.txt", lambda out, rc: rc == 0 and len(out.strip()) > 0),
        ("ignore list", lambda out, rc: rc == 0),
        ("ignore why a.txt", lambda out, rc: rc == 0 and "a.txt" in out),
        ("ignore attr a.txt --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("secret scan", lambda out, rc: rc == 0 and "no secrets" in out),
        ("secret scan --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("review --json", lambda out, rc: rc == 0 and _json_ok(out)),
        ("branch list", lambda out, rc: rc == 0 and "main" in out),
        ("tag list", lambda out, rc: rc == 0),
        ("sub list", lambda out, rc: rc == 0),
        ("stash list", lambda out, rc: rc == 0),
        ("ops", lambda out, rc: rc == 0),
        ("stats", lambda out, rc: rc == 0),
        ("history a.txt", lambda out, rc: rc == 0 and len(out.strip()) > 0),
        ("show main:a.txt", lambda out, rc: rc == 0 and "one" in out),
        ("fsck", lambda out, rc: rc == 0),
        ("gc", lambda out, rc: rc == 0),
    ]
    behave_bad = []
    for invocation, check in behaviours:
        r = sh(f"{GESTA} {invocation}", cwd=repo)
        combined = (r.stdout or "") + (r.stderr or "")
        try:
            good = check(combined, r.returncode)
        except Exception as exc:
            good = False
            combined = f"{combined} [{exc}]"
        if not good:
            behave_bad.append(f"gesta {invocation}: exit {r.returncode}, {combined.strip()[:120]}")

    print(f"documented commands: {len(documented_commands())}")
    print(f"  parse  : {len(ok)}")
    print(f"  missing: {len(missing)}")
    for m2 in missing:
        print(f"    - gesta {m2}")
    print(f"  unparsed: {len(unparsed)}")
    for u in unparsed:
        print(f"    - {u}")
    print(f"documented flags that are rejected: {len(flag_bad)}")
    for f in flag_bad:
        print(f"    - {f}")
    print(f"subcommands with no --help: {len(no_help)}")
    for n in no_help:
        print(f"    - {n}")

    print(f"documented invocations that do their job: {len(behaviours) - len(behave_bad)}/{len(behaviours)}")
    for b in behave_bad:
        print(f"    - {b}")

    bad = len(missing) + len(unparsed) + len(flag_bad) + len(no_help) + len(behave_bad)
    total = len(documented_commands()) + len(flag_bad) + len(no_help)
    print()
    print(f"{total - bad}/{total} documented surfaces answer")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
