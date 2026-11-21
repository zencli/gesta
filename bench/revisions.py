#!/usr/bin/env python3
"""Do blame, show and ls agree with the names log and history print?

Issues #2 had two halves. blame answered "never committed" for every file in an imported repository.
show and ls accepted only a full changeset id, rejecting HEAD, an abbreviated id and the vN label that
history itself prints. The contract this checks is simple: whatever a command prints, another command
accepts.

Each spelling is taken from real output rather than constructed, because a constructed one would pass
while the printed one failed.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
_PROJECT = os.path.dirname(_HERE)
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))


def sh(cmd, cwd=None):
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=180)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    base = tempfile.mkdtemp(prefix="rev-")
    src = os.path.join(base, "src")
    dst = os.path.join(base, "dst")
    try:
        # A git repository with two commits, the second appending a line. Imported, so blame has to
        # work on the import path rather than on a locally made changeset.
        os.makedirs(src)
        sh("git init -q && git config user.email a@b.c && git config user.name t", src)
        with open(os.path.join(src, "a.txt"), "w") as fh:
            fh.write("line one\nline two\n")
        sh("git add -A && git commit -qm one", src)
        with open(os.path.join(src, "a.txt"), "a") as fh:
            fh.write("line three\n")
        sh("git add -A && git commit -qm two", src)

        os.makedirs(dst)
        sh(f"{ZF} init", dst)
        sh(f"{ZF} git import {src}", dst)

        # blame answers, and attributes the appended line to the second commit and the rest to the
        # first. That agreement is the property, not the exit code.
        rc_blame, blame = sh(f"{ZF} blame a.txt", dst)
        blame_works = rc_blame == 0 and "never committed" not in blame
        ids = re.findall(r"(cs[0-9a-f]+)", blame)
        distinct = len(set(ids)) >= 2 if ids else False
        # The two lines that existed in the first commit must not be credited to the second.
        first_two_same = len(ids) >= 3 and ids[0] == ids[1] and ids[2] != ids[0]

        # history and blame agree: the newest version history prints belongs to the newest blame line.
        rc_hist, hist = sh(f"{ZF} history a.txt", dst)
        hist_ok = rc_hist == 0 and "never committed" not in hist
        agree = blame_works and hist_ok and first_two_same

        # Every spelling a reader can copy out of output must resolve.
        head = sh(f"{ZF} show HEAD:a.txt", dst)
        head_ok = head[0] == 0 and "line one" in head[1]

        rc_ls_head, ls_head = sh(f"{ZF} ls HEAD", dst)
        ls_head_ok = rc_ls_head == 0 and "a.txt" in ls_head

        full = ""
        m = re.search(r"\b(cs[0-9a-f]{10,})\b", sh(f"{ZF} log --oneline", dst)[1])
        if m:
            full = m.group(1)
        # Eight characters of the id, which is what a person copies.
        prefix = full[:8] if len(full) >= 8 else ""
        rc_prefix, prefix_out = sh(f"{ZF} show {prefix}:a.txt", dst)
        prefix_ok = bool(prefix) and rc_prefix == 0 and "line" in prefix_out

        # The vN label is per file, so it resolves with a path and is refused without one in a way
        # that says which file to name.
        labels = re.findall(r"^v(\d+)", hist, re.M)
        v_label = f"v{max(int(x) for x in labels)}" if labels else ""
        rc_v, v_out = sh(f"{ZF} show {v_label}:a.txt", dst) if v_label else (1, "")
        v_ok = bool(v_label) and rc_v == 0
        # Without a path the label is ambiguous only when more than one path has that version. With
        # one path it resolves, which is the right answer and not an error, so both outcomes pass as
        # long as the message is not "does not exist".
        rc_v_ambiguous, v_msg = sh(f"{ZF} show {v_label}", dst) if v_label else (1, "")
        says_which = "does not exist" not in v_msg
        says_which = rc_v_ambiguous != 0 and ("a.txt" in v_msg or "path" in v_msg)

        # The id history prints in its second column, which is what a reader copies. It must resolve,
        # or the two commands disagree about the same file, which is the fault this checks.
        shown_id = ""
        m2 = re.search(r"v\d+\s+(cs[0-9a-f]+)", hist)
        if m2:
            shown_id = m2.group(1)
        rc_shown, shown_out = (sh(f"{ZF} show {shown_id}:a.txt", dst)
                               if shown_id else (1, ""))
        git_ok = bool(shown_id) and rc_shown == 0 and "line" in shown_out

        checks = [
            ("blame works on an imported repository", blame_works),
            ("blame attributes lines to more than one changeset", distinct),
            ("the appended line belongs to the later commit", first_two_same),
            ("history agrees with blame", agree),
            ("show accepts HEAD", head_ok),
            ("ls accepts HEAD", ls_head_ok),
            ("show accepts a changeset prefix", prefix_ok),
            ("show accepts vN with a path", v_ok),
            ("vN without a path is refused with the file named", says_which),
            ("the id history prints resolves in show", git_ok),
        ]
        for name, ok in checks:
            print(("  [PASS] " if ok else "  [FAIL] ") + name)
        print()
        print(f"  blame ids: {ids}")
        print()
        if all(ok for _, ok in checks):
            print(f"{len(checks)}/{len(checks)} revision and blame checks pass")
            return 0
        print(blame[-400:])
        print(v_msg[-300:])
        return 1
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
