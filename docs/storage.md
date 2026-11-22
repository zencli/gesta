# Why packing small objects did not help

A tree of 1000 tiny files costs git between **4.0x and 8.1x** in disk on the current build, depending
entirely on how much the files share. All three figures below are measured after each store's own
compaction: this tool vacuumed, git packed.

| Tree, 1000 files, distinct contents | gesta | git | ratio |
|---|---|---|---|
| every file different, nothing shared | 1,142,922 | 284,431 | **4.02x** |
| every file has the same 200-line prologue | 1,618,058 | 198,843 | **8.14x** |
| small JSON, same shape, different values | 1,122,442 | 260,183 | **4.31x** |

The shared-prologue case is the worst, and it is worth understanding why rather than treating it as
noise. git finds the 200 shared lines once and stores a delta for each file against the previous one.
This tool stores each body whole, so the prologue is paid 1000 times. The gap is therefore not a
constant to be quoted but a function of how much a tree repeats itself.

Earlier versions of this paragraph said 5.6x, and before that 14.9x. Both were single numbers for a
single synthetic shape: `tiny_files.py` writes 1000 copies of the same seven-byte body, which git
deduplicates to almost nothing, so the ratio it printed measured how well each tool collapses one
repeated blob. That is a real property and it is not the one a repository has. `bench/storage_real.py`
replaces it with the three shapes above.

The 14.91x figure was also wrong twice over on its own terms: the arithmetic did not follow from the
numbers printed beside it (856 against 103 is 8.3x), and the measurement counted the working tree on
both sides and compared this tool mid-WAL against a packed git.

Earlier figures of 6.6x and 7.7x below are the numbers as measured at the time of the experiment being
described, not current claims, and they carry the same caveat: they were taken before blame became
ranges and before the measurement was corrected.

The obvious fix is to pack several small bodies into one row, so the row header and the index entry
are paid once per thousand objects rather than once per object. That fix was built and measured, and
it made the repository **larger**: 6.6x became 7.7x at the time.

This records the measurement, because the reasoning was sound and the result was not.

## What the bytes actually are

Measured with `dbstat` on 1000 files of 7 bytes each, one changeset, before any change:

| Table and index | Bytes | Share |
|---|---:|---:|
| `file_versions` | 114,688 | 11.2% |
| `changeset_files` | 106,496 | 10.4% |
| `worktree_cache` | 102,400 | 10.0% |
| `objects` | 98,304 | 9.6% |
| `branch_tree` | 98,304 | 9.6% |
| `sqlite_autoindex_objects_1` | 86,016 | 8.4% |
| `blame` | 49,152 | 4.8% |
| (nine more, each under 4%) | | |
| **total** | **1,024,000** | |

The `objects` table is 9.6 percent of the file, not the whole problem. Its content column
holds 7 bytes per row and the row costs about 98, so the packing idea addresses roughly
91 kilobytes of a megabyte.

## What the packing did

The design added `object_packed(hash, seg_id, seg_offset, seg_len)` and
`object_segments(seg_id, body)`, moved each small body into a shared segment, and blanked
its `content`. The bodies really did shrink: 1000 contents that occupied 98,304 bytes
became one 1,959 byte compressed segment.

But a pointer is not free. `object_packed` costs 86,016 bytes for its rows plus 86,016 for
its primary key index, and `object_segments` adds its own. Net effect on the file:

| | Before | After |
|---|---:|---:|
| `objects` rows | 98,304 | 90,112 |
| `object_packed` + its index | 0 | 172,032 |
| `object_segments` | 0 | small, one row |
| **total file** | **1,024,000** | **1,196,032** |

Eight kilobytes saved on the content, 172 kilobytes spent pointing at it.

## Why the idea failed here

A pointer into a segment is a hash, two integers and a row header, which is more than the
seven bytes it replaces. Packing wins when the bodies are large enough that the row
overhead is small against them, and that is exactly when compression has already made the
saving. The workload where packing would pay, bodies of a few hundred bytes, is not the
workload that cost 6.6x when it was measured.

## What the numbers say to do instead

The cost is spread across six tables that each hold one row per tracked path:
`file_versions`, `changeset_files`, `worktree_cache`, `branch_tree`, `blame`, and
`objects`. No single one dominates, and each exists for a reason:

- `branch_tree` is the maintained tree that keeps `status` from rebuilding history.
- `worktree_cache` is the stat cache that keeps `status` from re-reading every file.
- `blame` is one row per line per version.
- `file_versions` is the rename and history chain.
- `changeset_files` is the change itself.

Packing `objects` while leaving five other per-path tables alone cannot move the ratio far.
A real fix is fewer rows, not smaller ones. Two candidates, neither implemented:

1. **One row per path instead of per version.** `file_versions` and `blame` both grow with
   history, so a repository edited a thousand times holds a thousand rows per path. Keeping
   the newest and a bounded chain would cut those two tables; it costs the ability to ask
   about any older version, which is what `history` does today.
2. **A denser row format.** SQLite's per-row overhead is what makes 7 bytes cost 98. Storing
   the tree as one serialized value per branch, the way git does, trades the ability to
   query a single path for a much smaller file.

Both are schema changes and both lose a capability. Neither has been done. The ratio in this
section was measured on small files before the blame table was rewritten as ranges; see the next
section, and `docs/git-parity.md` for the figure on a real tree (2.3x against a packed git).

## Blame was 90% of the database

Measured on this project: 118 files, 62,824 blame rows, one distinct changeset. The table repeated
the same changeset id, author and timestamp once per line of every file, and its two indexes doubled
the cost. `dbstat` put blame at 3,944,448 bytes plus 4,412,800 bytes of index, against 622,592 bytes
for the objects themselves. The disk figures this project reported as 6.6x and 10.2x were mostly this
one table, which is why both of those numbers were retired once the ranges landed: on 11,301 files
the table is now 0.9 MB, 1.2% of the database, and the remaining ratio is compression.

Blame is stored as ranges now. A file written by one commit is one row:

```
path            start_line  end_line  changeset_id
wide.txt        1           400       cs...
```

Editing one line splits it into three ranges rather than 401 rows. The author and the timestamp are
not stored here at all: they live on `changesets`, keyed by id, and a reader joins for them.

Two numbers, both measured.

| Tree | Before | After |
|---|---|---|
| this project, 118 files | 9,236,480 bytes, 62,824 rows | 901,120 bytes, 116 rows |
| a markdown vault, 10,669 files | 550,338,560 bytes | 73,469,952 bytes |

Against git's roughly 54 MB for the same vault, that is 1.36x instead of 10.16x. Committing the
whole vault took 5,371 ms before and 1,229 ms after.

The general rule: a row that repeats a value which has one home is the largest cost in a database,
and no amount of making that row smaller competes with not writing it.

## Re-measured, and what 11% buys

The 6.6x above was taken before blame became ranges. Measured again, 1000 files of seven bytes costs
864 KB, split across five tables that each hold one row per path:

| Table | Bytes | Share |
|---|---:|---:|
| `file_versions` | 110,592 | 12.9% |
| `changeset_files` | 106,496 | 12.4% |
| `worktree_cache` | 102,400 | 12.0% |
| `branch_tree` | 94,208 | 11.0% |
| `blame_ranges` | 40,960 | 4.8% |
| indexes (five) | 118,784 | 13.9% |

`objects` holds one row for all 1000 files, because seven identical bodies dedup to one. The cost is
not the object store at all; it is five rows of about a hundred bytes holding seven bytes each.

### The one change that measured worth doing

A hash is stored as 64 characters of hex. As a 32-byte blob it is half the width, and the saving across
`worktree_cache`, `branch_tree`, `file_versions` and `objects` measured at 94 KB on 864 KB: **10.9%**.

That is worth doing and has not been done, because every one of those tables is read and written
through a `String` and the change reaches roughly fifty call sites. An 11% saving does not justify
rewriting the type of the value the whole storage layer passes around, on a codebase where the tests
pass and the next change is expected to be small. It is recorded here so the next person to look at
disk usage does not have to measure it again.

`bench/hash_size.py` reproduces the 11% in seconds by rewriting the hash column of a copy into a blob
and comparing the vacuumed files. It changes nothing.

### What would actually move the number

The rows are the problem, not the bytes in them. Two candidates, neither implemented:

1. **One row per path rather than per version.** `file_versions` and `blame_ranges` grow with history.
   A repository edited a thousand times holds a thousand rows per path. Keeping the newest and a
   bounded chain would cut both; it costs the ability to ask about any older version, which `history`
   does today.
2. **A denser row format.** SQLite's per-row overhead is what makes seven bytes cost a hundred.
   Storing a tree as one serialized value per branch, the way git does, trades per-path queries for a
   much smaller file.

Both are schema changes and both lose a capability. A storage figure that is smaller because a feature
disappeared is not an improvement, and neither was done.
