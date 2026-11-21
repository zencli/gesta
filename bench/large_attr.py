#!/usr/bin/env python3
"""Does `large` in .gitattributes put an object beside the database?

The sidecar already takes anything past 4 MB. This checks the direction a threshold cannot: a path
declared large while it is still small. A 3 MB binary stored inline today is copied by every backup and
every VACUUM from then on, and by the time it is 300 MB that decision was made a year ago.

Four bodies in one commit, well under the threshold:

    x.big   ] identical bytes, declared large    ] dedup to one object regardless of the rule
    y.big   ]                                    ]
    p.big   different bytes, declared large      -> sidecar
    q.keep  different bytes, refuses the rule    -> inline

The identical pair is in the test because content addressing decides an object's name, and a name
cannot depend on which path reached it. A rule that changed that would mean the store had stopped
being content-addressed, which would be a much larger finding than the one this checks.
"""
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile

ZF = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta")

SAME = b"\x00DUPLICATE" * 2000      # 20000 bytes, two paths
BIG = b"BIG!" * 3000                # 12000 bytes
KEEP = b"KEEP" * 3000               # 12000 bytes


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def objects_of_size(db, size):
    """(hash, sidecar, size) for the object whose body is exactly `size` bytes."""
    conn = sqlite3.connect(db)
    try:
        return conn.execute(
            "SELECT hash, sidecar, size FROM objects WHERE size = ?", [size]
        ).fetchall()
    finally:
        conn.close()


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="large-attr-")
    try:
        sh(["init"], d)
        with open(os.path.join(d, ".gitattributes"), "w") as fh:
            fh.write("*.big large\n*.keep -large\n")

        for name, body in (("x.big", SAME), ("y.big", SAME), ("p.big", BIG), ("q.keep", KEEP)):
            with open(os.path.join(d, name), "wb") as fh:
                fh.write(body)

        made = sh(["changeset", "create", "one", "--author", "t", "--json"], d)
        m = re.search(r"(cs[0-9a-f]+)", made.stdout)
        if m:
            sh(["changeset", "commit", m.group(1)], d)

        db = os.path.join(d, ".gesta", "gesta.db")

        dup = objects_of_size(db, len(SAME))
        checks.append(("two identical bodies dedup to one object", len(dup) == 1,
                       f"{len(dup)} objects of {len(SAME)} bytes"))

        pair = objects_of_size(db, len(BIG))
        checks.append(("the two different bodies are stored separately", len(pair) == 2,
                       f"{len(pair)} objects of {len(BIG)} bytes"))

        sidecar = [r for r in pair if r[1] == 1]
        inline = [r for r in pair if r[1] == 0]
        checks.append(("the rule sent a small body out of the database", len(sidecar) == 1,
                       f"{len(sidecar)} sidecar, {len(inline)} inline"))
        checks.append(("the rule that refuses it kept one inline", len(inline) == 1,
                       f"{len(inline)} inline"))

        if sidecar:
            h = sidecar[0][0]
            p = os.path.join(d, ".gesta", "objects", h[:2], h[2:])
            checks.append(("the sidecar file is where the hash says", os.path.exists(p), p))
            if os.path.exists(p):
                with open(p, "rb") as fh:
                    body = fh.read()
                checks.append(("the sidecar holds the bytes", body == BIG, f"{len(body)} bytes"))

        # Reading it back must not care where it lives.
        r = sh(["show", "p.big"], d)
        checks.append(("show returns the sidecar body unchanged",
                       r.returncode == 0 and r.stdout == BIG.decode("latin-1"),
                       f"rc={r.returncode} {len(r.stdout)} chars"))

        # A file the rules do not name is decided by size, which is the fallback and has to keep working.
        with open(os.path.join(d, "plain.bin"), "wb") as fh:
            fh.write(b"P" * 5000)
        made = sh(["changeset", "create", "two", "--author", "t", "--json"], d)
        m = re.search(r"(cs[0-9a-f]+)", made.stdout)
        if m:
            sh(["changeset", "commit", m.group(1)], d)
        plain = objects_of_size(db, 5000)
        checks.append(("a path no rule names stays inline under the threshold",
                       len(plain) == 1 and plain[0][1] == 0,
                       f"{[(r[1]) for r in plain]}"))

    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} large-attribute checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
