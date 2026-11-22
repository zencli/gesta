# What still separates this from git

Written after replacing git in daily work on this machine, with each claim checked by running
something rather than by reading the code. The order is by what blocks the most.

## Closed in this round

- **Binary files.** They were refused outright, so any repository with an image was unusable.
  Stored raw now, carried in bundles, restored byte for byte.
- **Packfiles.** `git gc` moves every object into a pack and every branch into `packed-refs`.
  Both are read now, so importing a real repository works.
- **Signing.** Ed25519 over the changeset identity, with the private key refusing to be Debug so a
  panic cannot print it.
- **Listing the tree.** `ls`, which did not exist and is the first thing any caller needs.

## Not closed, in the order they block

### 1. Disk: 1.4x git on a real tree, 14.9x on a pathological one

Two numbers answer the disk question and they belong together, because quoting either alone misleads.
On 11,301 files holding 171 MB of content, both tools at their best, the ratio is **1.4x**. On 1,000
files of seven bytes each - a tree whose whole content is smaller than the schema that indexes it - the
ratio is **14.91x**, pinned by the `storage-cost` gate step at 18x. Per-row overhead dominates when
every object is a few dozen bytes; nothing about the first number makes the second go away.

The 11,301 file measurement, both tools at their best:

```
work tree                      171.7 MB
git, packed                     37.4 MB
gesta, after VACUUM             72.8 MB
```

The earlier figure in this document was 10.2x, and it dated from before the blame table was rewritten
as ranges. It also compared gesta against an unpacked git, and named a table as the suspect without
measuring it. `bench/disk.py` now does what a comparison has to do: pack git with `--no-detach`
(a plain `git gc` detaches and returns before it has packed anything, which made an early run of this
very script report that git never shrank), VACUUM gesta, and break the database down by table.

```
objects                  59.8 MB   82.1%
file_versions             1.7 MB    2.3%
changeset_files           1.6 MB    2.2%
worktree_cache            1.6 MB    2.1%
branch_tree               1.5 MB    2.0%
blame_ranges              0.9 MB    1.2%
every other table         5.4 MB    7.9%
```

Everything that is a row per path is small. The content is the cost, and the reason is specific:
git compresses many objects into one stream, and gesta compresses each object on its own. Measured
on this tree, joining the objects into one stream gives 1.50x, which is almost exactly the remaining
gap between 76.3 and 33.8 MB once the ratios are applied to the same content.

Raising the compression level does nothing: level 6 gives 47.3 MB and level 9 gives 47.2 MB, for
10% more time. The gap is not effort, it is that a zlib stream cannot see the next file.

That gap is closed now, and the shape of the fix came from measuring it first. Grouping objects
before compressing them, 50 at a time and sorted by size, gets 1.36x; joining everything into one
stream gets only 1.50x. Almost the whole win is available at a granularity a read can afford, so
there was no need to trade the object model for it.

`gesta gc` groups loose text objects into streams. Each object keeps its row, its hash and its own
identity; the row names the group and the byte range instead of holding the body. Measured on the
same 11,225 file tree:

```
before gc       69.2 MB
after gc        51.4 MB      (25% smaller)
```

A read decompresses one group, which is 50 objects rather than the whole store, and the group is
bounded on purpose. A repository too small to benefit forms no group at all, so a hundred small files
do not pay for a mechanism they do not need.

One thing this cost, and it is why the constant is 50: a read is now proportional to its group rather
than to one object. At 50 objects that is a few hundred kilobytes of decompression for a few kilobytes
of answer. At 1000 it measured almost no extra saving and a much worse read.

Measured on a 600 file tree, reading one file through `show`:

```
loose    1.6 ms median
packed   1.8 ms median      (1.12x)
```

So the read price is 12%, not the several-fold a pack format can cost when the group is a whole
repository. On a tree that small the pack does not pay for itself at all, which is why a repository
below the threshold forms no group: 847 KB loose against 995 KB packed, because the index entries
cost more than the shared vocabulary saved. The mechanism needs a tree with enough repeated text to
be worth having.

### 2. `.gitattributes` covers line endings and nothing else

`text`, `-text`/`binary`, `eol=lf` and `eol=crlf` are honoured, read from `.gitattributes` and from
`.gesta/attributes`, with the last matching rule winning. A text rule normalises to LF in the object
and writes the named ending to the working tree, so a Windows checkout and a Unix one agree on what
changed. `ignore attr <path>` says which rule decided, because a rule file where the last match wins
is easy to write and hard to read back.

Absent: filters, diff drivers, merge drivers, and `working-tree-encoding`. Each of those runs a
program on every file, which is a different feature with its own trust question.

### 3. A round trip now keeps every commit name

An import followed by an export used to produce a repository whose content matched and whose commit
ids did not: 0 of 97 survived on this project. git hashes the author line including its timezone, and
the export wrote `+0000` where the import had read `+0700`.

The timezone is recorded on the changeset now and written back on export. Measured after the fix:
97 of 97 commit ids survive an import and export of this project's own history, and `git fsck` accepts
the result.

The gate asserts this with a source repository written at `+0700`, because a `+0000` source matches
whether the code is right or wrong, which is how the fault survived a test that read the export back.

### 4. The server speaks enough of the git protocol to clone and to push

`gesta serve` answers the four requests a client makes: `info/refs?service=git-upload-pack`,
`POST git-upload-pack`, `info/refs?service=git-receive-pack` and `POST git-receive-pack`. A plain
git client clones, and a plain git client pushes.

That is the check, and the only one worth making: a test that reads our own response back proves the
response is well formed and nothing about whether git agrees with it.

Six details each produce a symptom that looks like a different bug:

- The pkt-line length counts its own four byte header.
- The first ref line carries the capabilities after a NUL, and only the first one.
- Under `side-band-64k` the stream ends with a band-1 packet carrying nothing, then a flush.
- `HEAD` has to be advertised as a ref of its own alongside `symref=HEAD:refs/heads/main`. Without
  the `HEAD` line git files everything under `refs/remotes/origin` and leaves the tree empty with a
  warning about a missing ref.
- A push sends a **thin pack**: the objects the server is known to hold are left out and named as
  bases. Unpacking into an empty repository fails on an object the client never sent, so the server's
  own history is exported into the scratch repository before the new objects arrive. This is the
  detail that separates a push that works from a push that reports a missing object.
- The client's `old` value has to be compared with the branch **before** the import moves it.
  Reading it afterwards compares the client's expectation with the result of applying the push, which
  always differs, and every correct push is rejected.

`index-pack` is not enough to unpack a push: it leaves the objects inside a pack, and the importer
reads loose objects. `unpack-objects` writes them loose, which is the shape the next step needs.

A fetch after the first one sends the change rather than the tree. The client names the commits it
holds as `have`, and every object those commits reach is left out of the pack. Measured on a sixty
file tree with one file edited: a full fetch is 7662 bytes, the same fetch carrying a `have` is 1739.
`bench/fetch_negotiate.py` posts both requests to a real server and counts what came back; the gate
runs it.

The parser had a bug that made this look unimplemented. `read_pkt_lines` stopped at the first flush,
and a fetch sends its `want`s, a flush, then its `have`s and `done`. Every `have` was dropped, so a
server that read the list behaved exactly like one that ignored it.

Protocol v2 is served, and `multi_ack_detailed` is negotiated. Both were listed here as absent for as
long as they were absent, and then for longer than that - the paragraph you are reading said they were
missing while `bench/protocol_v2.py` (5/5) and `bench/multi_ack.py` (6/6) were passing. A stale "not
implemented" is worse than no note at all, because it sends the next reader to rewrite something that
works.

- **v2** answers `ls-refs` and `fetch` with `acknowledgments`. The check forces the client to v2 with
  `-c protocol.version=2` and does a real clone and fetch: a v2 conversation the client cannot finish
  fails as a stall or as "fatal: invalid server response", and both look like a broken repository
  rather than a missing capability.
- **`multi_ack_detailed`** is advertised because the negotiation answers it. Measured: a fetch with a
  `have` returns 9 ACK lines across forms ``, `common` and `ready`. `bench/multi_ack.py` pins
  `protocol.version=0`, because git 2.43 defaults to v2 where `multi_ack_detailed` does not exist at
  all and an unpinned run would measure the other protocol and fail at the wrong seam.

Ref deletion works and is tested (`bench/gate.py git-push`).

### 5. No SSH transport

A remote is a filesystem path, an HTTP server, or S3. An SSH remote would need the protocol and key
handling. `git push` covers it in the meantime by shelling out, which is honest but depends on the
git binary being present.

### 6. The write is flat; the scan is not

Measured on one file edit at three repository sizes, `create` and `commit` apart:

```
  files   gesta create   gesta commit   git add+commit  write ratio
    500      13.2ms       11.4ms           6.2ms        1.86x
   2000      19.7ms       12.8ms          13.8ms        0.92x
   8000      48.2ms       14.7ms          21.6ms        0.68x
```

The write costs 1.28x more in a repository 16x larger, which is the property the project claims: the
cost follows the change. At 8000 files the write is cheaper than `git add && git commit` (14.7
against 21.6 ms).

The read costs 3.65x more over the same range, and that number is real rather than a bug to hide.
Answering "what changed" means stat-ing every tracked file, some 14 ms of the 48 at 8000 files, and
comparing the rest against a cache. Git pays the same shape of cost in `git status` (7.9 ms at 8000
files on the same machine, because its index stores the stat of every path).

A single figure for the two phases said "2.12x worse" about a floor that did not move. Both numbers
are reported now, and the gate asserts the write is flat and the scan was seen growing, so neither
can be made to look good at the other's expense.

### 100,000 files: the write holds, the scan does not

The write claim was measured to 8,000 files. At 100,000 it still holds, and the comparison to the
tool this replaces flips in one column and not the other.

| files | gesta scan | git status | gesta write | git add+commit |
|---:|---:|---:|---:|---:|
| 8,000 | 33.1 | 11.0 | 13.4 | 15.6 |
| 100,000 | 454.7 | 78.4 | 13.2 | 100.2 |

The write is flat: 13.4 to 13.2 ms while the tree grew 12x, against git going 15.6 to 100.2. Above
roughly 10,000 files a commit is several times cheaper here.

The whole path says the opposite. A commit is a scan and a write, and at 100,000 files:

| | gesta | git |
|---|---:|---:|
| scan + write, one file changed | 446.4 ms | 108.1 ms |

The scan is 5.8x slower than `git status` and it swallows the write advantage. Measuring only the
write would have reported a win on the phase nobody runs alone.

Where the 364 ms of `status` goes at 100,000 files, measured part by part rather than guessed:

| part | ms |
|---|---:|
| walk only (scandir + stat per entry) | 195.8 |
| plus a relative path per entry | 261.5 |
| cache load, 100,002 rows | 47.7 |
| hash map built from those rows | 12.3 |

There is no single culprit. The kernel floor for stat-ing 100,000 files is 186.7 ms, so the tool is
about 1.95x its floor, and the extra is work git does not do: an ignore rule per path, a relative path
string per file, a cache read, and a hash map. Each is defensible on its own. Together they cost more
than the write they were protecting.

Two things measured and rejected as the cause: sorting the 100,002 collected paths costs 8.1 ms, and
the walk order is already unsorted, so the sort is not free but it is not the problem either.

One idea was written here first and then measured and removed, which is worth recording because it
reads as obviously correct:

> Compare the directory's own mtime with the recorded one and reuse every entry under it when it has
> not changed.

It is wrong. A directory's mtime changes when an entry is created, removed or renamed inside it. It
does not change when a file inside it is rewritten. Measured on this filesystem: modifying an existing
file left the directory mtime untouched, twice, while adding and removing a file both changed it.
Adopting the idea would have made every edit to an existing file invisible to `status`, which is the
one change a person makes most often. The measurement took two minutes and would have cost a bug that
looks like data loss.

### The sort nobody asked for

`scan_files_excluding` sorted its result before returning it. No caller reads the order: the scan's
output becomes a map keyed by relative path, and the two tests ask whether a path is present. The sort
was 79 ms of a 244 ms walk at 100,000 files, because comparing paths compares components rather than
bytes.

Measured A/B on the same tree, two binaries, seven runs each:

| | status at 100,000 files | ratio to `git status` |
|---|---:|---:|
| with the sort | 388.0 ms | 5.23x |
| without it | 299.6 ms | 4.04x |

23% faster, and the removed code was doing nothing.

The walk fell to 164 ms, and the Python floor of 186.7 ms was taken to mean the stat calls were no
longer the cost. That was wrong as well, and two more measurements were needed to get it right:

| walk, same tree, best of three | ms |
|---|---:|
| `read_dir` only, no stat at all | 21.7 |
| `read_dir` plus `file_type()` per entry, stat only files | 122.2 |
| `read_dir` plus `metadata()` per entry | 118.0 to 223.3 |

So 21.7 ms is not the floor: a scan has to know each file's size and mtime, which is a stat per file,
and that is 122 ms. `file_type()` is free because readdir already reported the type, and calling
`metadata()` on a directory is a stat the tool did not need. An earlier comment in the walk said
`metadata()` "costs nothing beyond what read_dir returned", which the first row contradicts.

Directories are now typed from the directory entry and only files are stat-ed. That saved 8 ms, not
100, because this tree holds 501 directories among 100,505 entries, so there were only 501 stats to
remove. The lesson is the one this file keeps recording: the cost of a change is what the measurement
says it is, and the shape of the tree decides which of the two floors applies.

That measurement also corrected an earlier one. `sorted()` on a list of Python strings costs 8.1 ms,
and that number was taken to mean the sort was cheap. It was not: the Rust sort compares `PathBuf`
values, which walks components, and the two are not comparable. Measuring the thing in the wrong
language produced a wrong conclusion that survived two rounds of reasoning.

### What the four remaining ideas were worth

Four changes were listed here as the way to close the gap. Three were measured and kept, one was
measured and thrown away, and the measurement changed the order.

| change | status at 100,000 files | verdict |
|---|---:|---|
| walk returns `(rel, meta)` and builds the path from a prefix | 319.9 -> 303.5 ms | kept |
| reserve the cache map instead of growing it | 222 -> 173 ms | kept |
| take the hash out of the map instead of cloning it | +5.2 ms slower | reverted |

The first two together, measured against the published binary over 21 interleaved pairs:

| | median | p10 | p90 |
|---|---:|---:|---:|
| before | 302.0 ms | 288.4 | 352.2 |
| after | 274.3 ms | 262.3 | 305.5 |
| `git status` | 77.9 ms | 72.3 | 85.2 |

9% faster, 3.88x to 3.52x against `git status`.

Where the 274 ms goes: about 122 ms of stat-ing every file, which no scan can avoid, roughly 37 ms of
ignore rule and path building, 29 ms loading 100,003 cache rows, 21 ms in the loop that builds the
result, and under 1 ms pruning. The two largest items left are the stats themselves and the cache
load.

`HashMap::remove` costs more than `get` plus a clone of a forty byte string, which is the opposite of
what the count of allocations suggests. That one was measured before it was believed.

### The second stat per file

`diff_worktree` built a list of every committed path, sorted it, and then asked the filesystem whether
each one still existed. The scan had already stat-ed every path on disk, so that was a second stat per
file, and the list cost a clone and a sort of 100,000 strings.

A path the scan did not report is missing. Saying so directly removed 80 ms and is the same answer the
old loop gave, which a test now pins for a file deleted on its own and a file deleted with its
directory.

| | status at 100,000 files |
|---|---:|
| before | 279.2 ms |
| after | 256.7 ms |

26 ms, measured over 21 interleaved pairs, and 3.72x to 3.42x against `git status`. Profiling the whole
path is what found it: `committed_tree` was 41 ms, the scan 234 ms, and the rest of `diff_worktree` 67
ms of which most was that redundant stat.

### Reading these numbers

Absolute times moved between runs on the same machine, by more than some of the changes measured here.
The same binary reported 279, 239 and 216 ms on three runs an hour apart with the machine idle, and a
binary run from the bind mount rather than from `/tmp` was 50% slower. Two comparison mistakes were
made from that before it was noticed: a ratio assembled out of two separate runs, and an `git status`
timed in a directory that was not a git repository, which returns in 2 ms.

`bench/status_vs_git.py` is the answer. It alternates the two tools round by round so drift hits both,
refuses to run unless both directories are repositories of the kind it expects, copies the binary off
the bind mount, and prints the median with p10 and p90. Every ratio in this file is meant to come from
a run of it.

Two runs of it, an hour apart:

| | gesta median | git median | ratio |
|---|---:|---:|---:|
| run 1 | 214.5 ms | 58.1 ms | 3.69x |
| run 2 | 215.3 ms | 58.8 ms | 3.66x |

Those agree, which the numbers assembled by hand did not.

### Why the maps are still there

Both tables have a unique index on `path`, so `ORDER BY path` is free: measured 3.2 ms extra over the
natural order for `worktree_cache` and 0.5 ms *less* for `branch_tree`, because the index answers the
query. A merge of two ordered streams would therefore need no map at all.

The arithmetic, measured rather than guessed:

| | cost |
|---|---:|
| map build, `worktree_cache` | 14.6 ms more than streaming the rows |
| map build, `branch_tree` | 12.4 ms more than streaming the rows |
| sorting 100,000 relative paths | about 7 ms |
| merging two sorted lists | about 3 ms in Rust |

That is about 9 ms saved out of 214, which is 4%, in exchange for replacing two lookups with a merge
whose failure mode is a wrong `status`: a merged scan that drifts by one entry reports the wrong file as
added or deleted, silently. Four percent is not worth that, so the maps stay.

Recording the arithmetic matters more than the decision. The next person to read this will wonder why a
free `ORDER BY` was not used, and the answer is a measurement, not a preference.

### Deleting a branch through the protocol

The server advertised `report-status` and nothing else, with a comment saying `delete-refs` was not
advertised because a deletion reported a result the code never produced. That comment described the
state of the code and not a decision: a push carrying no objects answered `ok` for every command
without doing anything, so git printed `- [deleted] throwaway` and `git ls-remote` still listed it.

Two things were wrong and both were the same kind of wrong:

- The empty-pack path answered `ok` for commands it never read.
- With that fixed, deleting the last branch emptied the repository of branches, and every command that
  needs one then reports `there are no branches yet`. A client is not in a position to judge that, so
  the server refuses it the way it refuses deleting a checked-out branch locally.

`delete-refs` is now advertised, a zero new id is a deletion, the `old` value is checked before
anything is removed, a token scoped to other branches is refused, and the last branch survives. The
gate runs a real git client through all of it:

| step |
|---|
| a client sees every branch the server has |
| deleting a branch through git is accepted |
| and the branch is gone from the server, not just from the client |
| and the other branches are untouched |
| one branch is left after deleting the others |
| and the only branch left cannot be deleted |

### A branch as a drive

`gesta mount X:` presents a branch through WinFSP, so a tool that only knows how to open files can
read a repository that has no checkout. Read-only on purpose: a writable view has to answer what a
write means when the branch is not checked out, which is a merge policy, and a filesystem that
silently discards a write is worse than one that refuses it.

It took five wrong attempts, and every one of them produced a mount that reported success and served
nothing. The trace that found it logged each callback to a file, which showed the driver calling
`get_security_by_name` and then stopping:

| symptom | cause |
|---|---|
| mount attaches, every request fails | `get_security_by_name` returned attributes 0, so nothing looked like a directory |
| `open` called in a loop, no `read_directory` | `open` never filled the `FileInfo` the driver reads back |
| the same, again | the normalised name was never set |
| listing returns bytes, Windows says "Incorrect function" | every entry had zero timestamps, which is 1601 |
| writes refused with "Incorrect function" | the trait's default `flush` returns `STATUS_INVALID_DEVICE_REQUEST` |

The one that mattered most was the first: a zero attribute is not "unknown", it is "not a directory and
not a file", and Windows stops there. `fsptool lsvol` showed the volume the whole time, which is why
"mounted" and "working" are different claims.

Two Windows quirks are worth recording because they cost time and are not in the crate's docs:

- The crate types `FileInfo::file_attributes` as `u32` and does not re-export the `FILE_ATTRIBUTE_*`
  constants, so they are named by value in `vfs.rs`.
- `OpenFileInfo` exposes `FileInfo` through `AsMut`, not through a `file_info_mut` method.

`cmd /c dir X:` still reports "Incorrect function" while `Get-ChildItem` works. That is `dir` asking
for volume information a WinFSP volume does not answer, not the filesystem refusing. The gate checks
through PowerShell for that reason.

Five gate steps run against a real mount:

| step |
|---|
| a branch can be mounted as a drive |
| and a file read through the mount has the bytes the store holds |
| and a nested path resolves |
| and the listing carries the size the store recorded |
| and a write is refused rather than silently dropped |

### Forwarding the record

The audit record is a file, which answers "what happened on this machine" and cannot answer "what
happened across the fleet" without somebody reading every file. `gesta serve` now posts each event to a
collector as it is written, so a refusal is visible while it is still worth acting on.

Configured by two files rather than flags, because a collector that moves should be a file edit and not
a change to every command that starts a server:

- `.gesta/audit-url` holds the URL.
- `.gesta/audit-token` holds the bearer token, in its own file so the URL, which every proxy logs, never
  carries a credential.

Three properties decide whether this is safe to run in production, and each is a test:

| property | why | how it is checked |
|---|---|---|
| a request never waits for the network | a slow collector must not slow a served request | 50 sends against a dead port finish in under 200 ms |
| a full queue is counted, not grown | an unbounded queue is a memory leak with a security-shaped name | the depth is bounded and `send` reports the drop |
| the token is not in the URL | a URL is logged everywhere it passes | the end-to-end test asserts the header and the absence from the URL |

The local file is written first and unconditionally. A collector being down must not cost the record on
the machine that produced it, because the file is what an operator reads when the collector is the thing
that broke.

`bench/audit_e2e.py` runs the real server against a real collector: a refused request, a served one, the
token on the wire, and the token absent from the URL.

Where the 206 ms goes at 100,000 files, measured by timing each phase rather than inferred:

```
walk (readdir, stat, ignore rules)   102 ms      stat alone 68, readdir 17, rules 12
load the stat cache                   25 ms
build the committed tree              30 ms
compare the two                        19 ms
```

The stat was the largest single item and it was serial, which is the difference from `git status`:
git preloads the index across threads and this did not. The walk is now two phases. Readdir collects
path and relative name; the stats then run over four threads in contiguous slices, with no lock on
the hot path.

Measured on this container, 100,003 files:

```
threads   stat phase
      1       78 ms
      2       43 ms
      4       24 ms
      8       34 ms
```

Four is where the curve flattens. Past it the cost is thread handoff on very short items, and a queue
with a lock measured 31 ms against the 24 that a slice per thread gets.

A whole `status` went from 205.6 ms to 183.4 ms, and the ratio against `git status` from 3.63x to
3.19x. The remaining gap is two 100,003 row reads, which is row parsing rather than anything a thread
can help with: a joined read answering both in one statement measured 105.9 ms against 106.1 for two
separate ones.

Attempts that did not move it, each measured:

- `PRAGMA mmap_size` and a larger page cache: 49.1 ms against 50.5 ms for the same 100,003 row read.
  The cost is parsing rows and allocating strings, not reading pages.
- Reserving the committed-tree map from the table's own count: 219.7 ms to 205.6 ms for a whole
  `status`. Real, and small.
- `FxHash` instead of the default hasher on the path maps: 208.0 ms against 205.6, inside the spread
  of the runs. Reverted. The keys are strings this program built and compared with itself, so the
  collision resistance bought nothing, and replacing it bought nothing either.

What is still open, and why it is a rewrite rather than a patch:

- Compare a directory's listing against the cache without materialising 100,003 map entries at all.
  That is the shape `git status` uses, with a binary index it can memory-map. Doing it here means a
  second cache format kept in step with the table, which is a decision about the store rather than a
  loop to tighten.
- Avoid loading cache rows for paths the walk will not visit. The two passes are separate today, so a
  tree where most cached paths are gone pays for rows it never reads.

The number above is what the tool does today: 3.19x `git status` on the same tree, down from 3.63x.

### Many writers in one checkout

SQLite takes one writer at a time, so the question is not whether writes serialise but whether the
store survives the queue. Measured with writers started together:

| writers | landed | wall ms | median ms | p95 ms | changesets | fsck |
|---:|---:|---:|---:|---:|---:|---|
| 20 | 20 | 309 | 52 | 135 | 21 | clean |
| 100 | 100 | 1185 | 172 | 307 | 101 | clean |

Every writer landed exactly one commit and `fsck` was clean at both sizes. The drain time grew 3.83x
for 5x the writers, which is what a serialised queue costs when the work per write is small.

The measurement found a real bug before it found a number. Writers that snapshot the whole tree
record each other's files: at 100 writers one changeset held 39 files belonging to other writers, and
one writer was told `changed: false` because the file it had just written had already been committed
by somebody else. Nothing was lost, but authorship was wrong, which for an agent surface is worse
than losing a commit because it looks correct.

`changeset create --path` limits the snapshot. A path matches itself or a parent of itself, so naming
a directory covers what is under it. Without the flag the snapshot is still the whole tree.

`bench/many_writers.py` runs the measurement; `bench/gate.py writer-scope` checks the scoping.

### Long histories: measured to 10,000 commits

A commit is billed as costing what the change costs, not what the repository holds. That claim was
measured on the file axis and not on the history axis, and when it was measured the second one was
false.

| commits | write ms | scan ms | db MB |
|---:|---:|---:|---:|
| 100 | 13.6 | 12.0 | 0.3 |
| 1,000 | 19.4 | 16.7 | 1.3 |
| 10,000 | 47.1 | 17.4 | 82.7 |

The scan stayed flat. The write did not: 3.47x the cost over 100x the history.

The cause was blame. Every commit deleted the file's whole `blame_ranges` and wrote it back, so the
cost of committing one line followed the number of lines the file already had. On a ten thousand
line file that delete and rewrite measured 34 ms, which is almost all of the growth.

The fix compares the new ranges with the stored ones and rewrites only from the first difference
onwards. Appending a line leaves every earlier range untouched.

| commits | write ms | scan ms |
|---:|---:|---:|
| 100 | 14.1 | 12.0 |
| 1,000 | 19.0 | 15.0 |
| 10,000 | 29.4 | 16.4 |

2.08x over 100x history, down from 3.47x.

Two mistakes on the way, both worth keeping:

- Adding an index on `file_versions(path, version DESC)` looked like the fix and was not. The lookup
  with `LIMIT 1` was already 0.004 ms. An index that helps nothing costs a write on every insert, and
  the measurement said so before the change was kept.
- The first version compared line by line against the previous version, which is wrong the moment a
  line is deleted: every line after it shifts, so index i is a different line. The comparison has to
  be between ranges, which is what the table holds. A test that deletes a line that a previous commit
  inserted caught it.

`bench/long_history.py` runs the full measurement; `bench/gate.py long-history` runs a short version
in the gate.

### 8. No interactive conflict resolver

Conflicts are data: both sides, line numbers, context, all three whole files. An agent writes the
answer. A human is handed conflict markers. That is a deliberate design choice, not a gap, but it
is a difference from git that a person will notice.

### 9. What git has that nothing here replaces

- **`.git/hooks` compatibility.** Six hook points run here (`pre-changeset`, `pre-commit`,
  `post-commit`, `pre-land`, `post-checkout`, `pre-push`), and each one is a program in
  `.gesta/hooks/`. A hook written for `.git/hooks` is not picked up, and the argument a git hook
  receives is not the same shape.
- **Submodules** in git's sense. `submodule` reads and writes `.gitmodules` and records a pointer
  rather than a copy, but the pointer lives in `.gitmodules`, not as a gitlink entry in the tree, so a
  git client reading the repository does not see the submodule at all. A child of this kind works; a
  child that is a git repository works through the git binary.
- **A hunk picker.** `stash save --hunk <path>:<n>` takes one hunk by number, which covers the case
  that matters: two unrelated edits in one file. There is no interactive picker and no way to take a
  partial line; a hunk is the finest grain, and splitting an edit that crosses a line is a manual job.
- **The forge ecosystem.** GitHub, GitLab and the rest speak git's protocol. `git push` is the
  bridge, and it is the one place a repository still needs the git binary.

Entries that used to be on this list are done, and are recorded here rather than deleted so the next
reader can tell they were considered: `bisect run` drives a whole search from one command (exit 0 good,
else bad, 125 skip), `worktree` gives a second working tree over one store, `notes` attaches an
annotation to a changeset without changing it, `stash save --path` sets aside named files without
disturbing the rest of the tree, and `stash save --hunk` goes one level finer. What is left of that last
one is the picker itself, which is listed above.

## What "100% replacement" would mean, honestly

For one person on one machine, on text and binary files, with a remote over HTTP or object
storage: nothing above blocks it today, and the daily cycle is measured and working.

For a team: disk at 1.4x on a real tree and 14.9x on a tree of tiny files, no forge of its own, and
no submodules in the git sense.
Those three are the real answer to the question, and two of them are schema and format work rather
than new features.
