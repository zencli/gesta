#!/usr/bin/env python3
"""What would storing hashes as 32-byte blobs instead of 64-char hex actually save?

Before changing a schema that every table shares, measure the ceiling. If the saving is a few percent
of the file, the change costs more in migration risk than it returns, and the answer is to write that
number down rather than to do the work.

This is a pure measurement: it takes the current database, rewrites the hash column of one table into
a blob in a copy, vacuums both, and compares. No code change, no migration.
"""
import os
import shutil
import sqlite3
import sys
import tempfile

SRC = "/tmp/t7/.gesta/gesta.db"

TABLES = {
    "worktree_cache": "hash",
    "branch_tree": "hash",
    "file_versions": "hash",
    "objects": "hash",
}


def total(path):
    c = sqlite3.connect(path)
    try:
        rows = c.execute("SELECT SUM(pgsize) FROM dbstat").fetchone()
        return rows[0] or 0
    finally:
        c.close()


def convert(table, col, work):
    """Copy one table into the same database with the hash column as a 32-byte blob."""
    c = sqlite3.connect(work)
    try:
        cols = [r[1] for r in c.execute(f"PRAGMA table_info({table})").fetchall()]
        if col not in cols:
            print(f"  {table} has no {col}")
            return False
        select = ", ".join(
            f"unhex({col}) AS {col}" if x == col else x for x in cols
        )
        c.execute(f"CREATE TABLE {table}_blob AS SELECT {select} FROM {table}")
        c.execute(f"DROP TABLE {table}")
        c.execute(f"ALTER TABLE {table}_blob RENAME TO {table}")
        # Not unique: several paths can hold the same content, which is the point of dedup.
        c.execute(f"CREATE INDEX idx_{table}_{col} ON {table}({col})")
        c.commit()
        return True
    finally:
        c.close()


def main():
    if not os.path.exists(SRC):
        print(f"no database at {SRC}; run bench/tiny_files.py first")
        return 1

    d = tempfile.mkdtemp(prefix="hashsize-")
    try:
        base = os.path.join(d, "base.db")
        shutil.copy(SRC, base)
        before = total(base)

        work = os.path.join(d, "work.db")
        shutil.copy(SRC, work)
        changed = []
        for table, col in TABLES.items():
            if convert(table, col, work):
                changed.append(table)
        c = sqlite3.connect(work)
        c.execute("VACUUM")
        c.close()
        after = total(work)

        print(f"as stored         {before:>9,} bytes")
        print(f"hashes as blobs   {after:>9,} bytes")
        saved = before - after
        pct = 100.0 * saved / before if before else 0
        print(f"saved             {saved:>9,} bytes  ({pct:.1f}%)")
        print(f"converted: {', '.join(changed)}")
        print()
        if pct < 5:
            print(f"{pct:.1f}% is not worth a schema migration that touches every hash in every table.")
            print("Record the number and leave the format alone.")
        else:
            print(f"{pct:.1f}% is large enough to be worth the migration.")
        return 0
    finally:
        shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
