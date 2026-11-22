# Speed, measured

Every number here is wall clock on this machine, measured by `bench/throughput.py` against git on the
same tree at the same moment. Nothing is estimated. Re-run that file on your own machine before
believing a figure, because the absolute numbers depend on disk and on whether the tree is in the page
cache.

## The short version

A commit costs about **4x** what git costs. A status costs about **1.5x** at 500 files and **2.4x** at
2000. `blame` and `log` are level with git, and `blame` is often faster.

The commit gap is a design cost, not a defect, and it is worth knowing what buys it:

| What happens on a commit | Why it is there |
|---|---|
| Read the previous version's object | to compute a delta and to attribute lines |
| Line diff against the previous version | so `blame` answers without walking history later |
| Write blame ranges for the changed lines | the same work, done at commit time rather than at read time |
| Store a delta, not a full body | to keep a long editing history small |

Git defers all of that: `blame` walks history when asked, and packs are built later by `gc`. So git
pays at read time and at collection time, and this tool pays at write time. Neither number alone says
which is better; a session that writes a thousand times and never reads blame is a session where this
tool's choice is the wrong one.

## The numbers

500 files, one file edited per operation:

```
status, 500 files clean            gesta  3.4 ms | git  2.1 ms |  1.58x slower
status, 1 file edited              gesta  3.6 ms | git  2.7 ms |  1.32x slower
diff, 1 file                       gesta  3.3 ms | git  2.1 ms |  1.54x slower
commit, 1 file                     gesta 19.9 ms | git  4.8 ms |  4.18x slower
log, last 10                       gesta  1.8 ms | git  1.7 ms |  1.05x slower
blame, small file                  gesta  2.1 ms | git  2.2 ms |  0.94x faster
```

2000 files:

```
status, 2000 files clean           gesta  8.3 ms | git  3.5 ms |  2.36x slower
status, 1 file edited              gesta  8.0 ms | git  3.2 ms |  2.48x slower
diff, 1 file                       gesta  8.2 ms | git  3.1 ms |  2.62x slower
commit, 1 file                     gesta 23.2 ms | git  7.8 ms |  2.99x slower
log, last 10                       gesta  2.1 ms | git  1.9 ms |  1.11x slower
blame, small file                  gesta  2.1 ms | git  3.1 ms |  0.67x faster
```

## What the commit cost depends on

Measured separately, so the shape is visible:

| Tree | Commit |
|---|---|
| 1 file | 30 ms |
| 100 files | 28 ms |
| 500 files | 28 ms |

The cost does not follow the size of the repository. That is the property worth having: a commit in a
repository of 10000 files costs what a commit in a repository of one file costs, which is the reason
the branch tree is stored rather than rebuilt.

It does follow the size of the file being committed, because blame is a line diff:

| Lines in the changed file | Commit |
|---|---|
| 10 | 30 ms |
| 500 | 28 ms |
| 2000 | 43 ms |

A commit that touches one line of a 2000 line file pays for a diff of 2000 lines. That is the one
place this design is quadratic where git is not, and it is worth knowing before committing a generated
file.

## Startup

A command that does no work at all costs about 2 ms: open the database, read the schema version, close.
`log --limit 1` is 3 ms and `status` on an unchanged tree is 3 ms, which is the floor. Nothing in the
figures above is process startup; all of it is work.

## What is not measured here

- Concurrent writers: `bench/many_writers.py` covers the shape, not the absolute cost.
- Network operations: `bench/http_remote.py` and `bench/tls.py` cover correctness, not speed.
- Rebuilding blame for a whole imported history: `bench/rebuild_cost.py` measures the slope and the
  numbers are printed rather than asserted, because the cost depends on how many versions a path has
  and there is no single threshold that would be true for every repository.
