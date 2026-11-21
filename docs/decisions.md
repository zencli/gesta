# Decisions

Every choice that shaped this program, with the reason it was made. Each one can be revisited.
The numbers are for reference in a discussion, not an ordering.

This is not a changelog. A changelog says what changed; this says why the shape is what it is.
Where a decision was reversed or a diagnosis was wrong, that is recorded too, because a decision
without its mistakes reads as if it were obvious.

## Shape of the program

| # | Decision | Reason |
|---|---|---|
| S1 | One binary, no daemon, no service, no account. | An agent on a laptop is the target. Anything that has to be started is something that can be down. |
| S2 | SQLite as the store, bundled so no system library is needed. | One file to copy, one file to back up, and transactions for free. A directory of loose object files cannot give an atomic commit without its own locking layer. |
| S3 | Every command works without a network. | A repository is local first. Remotes are transport, not the model. |
| S4 | The cost of a commit follows the size of the change, not the size of the repository. | The rule everything else bends around. A tree rebuilt per commit is the difference between an agent committing forty times a task and not committing at all. |

## The cost rule

| # | Decision | Reason |
|---|---|---|
| C1 | `set_branch_head` updates the branch tree incrementally from the changeset's own file list. | The one operation an agent does most must not cost the size of the repository. Measured with `bench/scale.py` (median of three rounds, binary read from the container's own disk): an ordinary one file commit goes from 28.7 ms at 500 files to 54.3 ms at 8000, which is 1.89x for a repository 16 times larger, with the middle half of samples inside 5 to 12 percent. |
| G1 | Git import and export are written against the documented object format, not by calling the git binary. | A bridge that shells out cannot run where git is not installed, which is the machine an agent works on. A round trip proves the format is right: equal commit ids mean the tree, the parents, the author line and the message all match, because the id is the hash of those four. |
| G2 | An import records the git commit id it came from. | Importing the same repository twice adds nothing, and a later export can tell which changesets came from git. |
| C1b | The rebuild is kept, and costs the size of the repository. | A merge, a land and a remote apply cannot carry the tree forward, so they rebuild it from history. Measured with `bench/rebuild_cost.py`: 22.7 ms at 500 files, 57.2 ms at 2000, 187.8 ms at 8000, which is 8.26x for 16 times the files. That is the price of the correctness the fallback buys, and it is why the common path does not take it. |
| C2 | The incremental path falls back to a rebuild when the changeset's parent is not the previous head. | Merge, undo and remote apply all re-parent. Updating incrementally across a re-parent would silently write a wrong tree. |
| C3 | Deltas are capped at a chain of 16. | An unbounded chain makes a read walk the whole history. The cap bounds a read and keeps the compression that matters. |
| C4 | `status` reads a maintained table instead of rebuilding the committed tree. | Rebuilding ran on every `status` and every `diff`, so the saving lands on the most frequent command. The 2 ms figure from the original change has not been reproduced since, and a `status` on 500 files now measures about 30 ms in total, which the note below explains is not comparable. A number that cannot be reproduced is recorded as such rather than repeated. |
| C5 | The stat cache exists, and a file younger than a small window is re-read rather than trusted. | A cache hit needs size and mtime to match. A file written in the same clock tick as the last read has an mtime that proves nothing. |

## Evidence over claims

| # | Decision | Reason |
|---|---|---|
| E1 | `fsck --worktree` hashes every tracked path and compares it to the disk. | A check that reads the database and not the disk is half a check. It is opt in because it reads every file. |
| E2 | `fsck --worktree` never reports a file the user made, a path outside the sparse scope, or a branch that is not current. | A check that cries wolf is turned off. Three tests hold that line. |
| E3 | A revert raises a change's risk score and never lowers it. | A rule that can quietly hide a change someone wanted to see is worse than a noisy one. |
| E4 | `land` materialises the working tree when the target branch is the checked-out one. | Without it the head moved and the disk did not: `show main:a.txt` returned the new content while `status` called the file deleted. |
| E5 | A shallow clone carries a snapshot of the head tree inside the bundle. | Truncating the history is exactly removing the changesets that created the old files. Restoring from the chain gave 5 files out of 200. |
| E6 | Bundle completeness counts the snapshot and every object the snapshot names. | A head that had not moved left its objects out of the bundle, so the bundle contradicted itself. |

## Things that must not be lost

| # | Decision | Reason |
|---|---|---|
| K1 | A stash records a deletion as a row with no hash. | Collecting only added and modified paths reported "nothing to stash" when the only change was a removal, and the intent to delete disappeared on the next branch switch. |
| K2 | A three way merge refuses to append a replacement that is already the tail of what was emitted. | Two sides expanding adjacent lines to the same content produced the line twice with no conflict reported. |
| K3 | Merge overlap is decided by the content the regions produce, not by where they sit. | Two wrong fixes came first: skipping hunks inside an already handled region broke a merge into an empty base, and treating a partial overlap as a conflict never fired because the regions are adjacent. |
| K4 | `undo` restores the working tree and the database row together. | An undo that leaves the disk ahead of the head is a silent divergence. |
| K5 | `--fix` refuses anything a person's judgement could change, and names the command. | A fix that silently skips work is worse than no fix, because the caller believes the state is clean. |
| K6 | Setting uncommitted work aside takes an explicit flag. | Committing a snapshot is safe because the snapshot describes the disk. A stash is safe because it keeps the work. Removing someone's unfinished edit from the tree is not safe, so it is never the default. |

## Secrets and transport

| # | Decision | Reason |
|---|---|---|
| P1 | A secret never appears in `argv`, in a URL, or in the repository. | `argv` is readable through `ps` and lands in shell history. A URL is logged by every proxy on the path. |
| P2 | The secret store is one file per user, not per repository. | A repository can be copied or pushed; the credential must not travel with it. |
| P3 | Credentials are read from the environment first, then the sealed store. | The environment is what a test or a one-off job sets; the store is what a person sets once and forgets. |
| P4 | The sealed store uses ChaCha20-Poly1305 with a single pass KDF. | The threat model is a leaked backup, not an attacker holding the file and a GPU. The single pass is stated rather than hidden. |
| P5 | `S3Config` has no `Debug`. | It holds the secret key, and a derived `Debug` prints it in any panic message. The test avoids needing it rather than adding it for convenience. |
| P6 | A plain `http://` endpoint is refused except on loopback. | The signature names the key, covers the payload and can be replayed inside its signing window. Loopback is allowed because a local test server cannot be read by anyone else. |
| P7 | The S3 remote is kept. | It is the only remote that needs no listening process. It costs 719 lines across `s3.rs` and `sigv4.rs` and one gate step. Measured from two builds, the TLS feature adds 1.63 MB (6,065,400 to 7,695,864 bytes), and that cost belongs to the HTTPS remote, which needs it on its own; S3 reuses it. |

## The interface an agent meets

| # | Decision | Reason |
|---|---|---|
| A1 | `brief` returns a named state, not a paragraph. | A program branches on `commit-work`; a summary gets read once and forgotten. |
| A2 | A stopped rebase outranks everything in `brief`. | Nothing else is safe until it is finished. |
| A3 | New work beyond a snapshot outranks committing that snapshot. | The snapshot does not describe the new file, so committing it would drop the file. |
| A4 | `grep --history` reads each distinct version once. | Reading per changeset repeated the whole history, which is the difference between a search that finishes and one that does not. |
| A5 | Search and state are exposed over MCP as well as from a shell. | The protocol is how an agent reaches the tool. A capability only a shell can reach is a capability the agent effectively lacks. |
| A6 | Every JSON object carries `schema: gesta@1`, including MCP results. | A renamed field breaks a caller silently: it keeps running and reads the wrong value. |
| A7 | A top-level list stays a bare list, and `--envelope` asks for the versioned object. | A list cannot carry a key, and wrapping it would break the pipes people already write. |
| A8 | `grep --regex` is a separate feature from the default build. | A build that only needs literal search keeps a smaller binary. A pattern the engine cannot express is refused with the construct named. |

## Install and delivery

| # | Decision | Reason |
|---|---|---|
| I1 | Install is one command, and a prebuilt binary is tried before asking for a toolchain. | The machine that most needs an installer is the one without the compiler. |
| I2 | The Windows installer targets PowerShell 5.1 and writes the user PATH. | Every Windows 10 and 11 machine has 5.1. Elevation is why installs get abandoned. |
| I3 | No releases, no tags. | The project ships from the branch. A version number in the manifest tracks the interface, not a packaged artifact. |

## How a number gets here

Every figure in this document carries what it takes to judge it: the command that produced it,
the spread, and whether it can be reproduced. A number without those three is a claim, and a
claim in a decision table is worse than a blank, because it gets quoted.

Three rules came out of getting this wrong repeatedly:

- **Measure the thing, not something near it.** Four attempts at measuring the tree rebuild each
  timed a different operation: disk writes, a growing table, a conflict, a branch switch. Each
  produced a plausible curve.
- **Hold one variable.** At 500 files, `land` took 29, 20 and 30 ms with 5, 50 and 200 changesets
  of history. That settled in one run what four rounds of reasoning had not: the cost follows the
  files, not the length of the history.
- **Say when a number is old.** The 2 ms for rebuilding the tree in C4 has not been reproduced and
  is marked as such. It may well be right; nobody has shown it.

## Mistakes kept on the record

These cost more time than the decisions above, so they stay written down.

| # | What happened | What it taught |
|---|---|---|
| M1 | A fixture set `parent = NULL`, so the incremental tree path was never exercised by any test. | A test that does not walk the code path it names proves nothing. |
| M2 | `commit_one` re-parented the changeset before calling `set_branch_head`, so the condition that enables the incremental path was always false. | The bug was in the order of two lines, and every test passed. |
| M3 | Two merge "fixes" were wrong before the third was right. | Compute the hunk positions by hand before changing merge logic. A few lines of Python settled in a minute what two rounds of guessing did not. |
| M4 | Four tests shared one temporary directory because the name used the process id. | A fixture that collides across threads produces failures that look like product defects. |
| M5 | An investigation blamed a traversal and a diff function for missing files; both were innocent. | Check the fixture before believing the diagnosis. `status --json` on a clean fixture settled it in one call. |
| M6 | `--branch main` was used with `cargo install --git`, which fails because the remote has no such tracking ref. | Run the installer from the published URL, not from a checkout. Three defects only appeared that way. |
| M7 | `bench/scale.py` kept the first binary it copied to `/tmp` and reused it on every later run. | A benchmark that reuses a stale copy measures the previous build and reports it as the current one. The copy is refreshed every run now, and the numbers above are from a correct run. |
| M8 | Timing the binary in place added about 16 ms per run, because it lives on a bind mount. | Read an executable from the container's own disk before timing it. The mount cost was larger than the work being measured, and it looked exactly like slow code. |
| M9 | The benchmark printed one median per size, and ten runs of the same commit on this machine ranged from 29 ms to 50 ms. | Report the spread beside the number. A single figure from a 70 percent range cannot support a claim, and the person reading it has no way to tell. The report now prints the median of three rounds and the interquartile spread of each. |
| M10 | Measuring the tree rebuild took four attempts, each wrong in a different way. | `undo` writes every path to disk, so it timed the file system. A fresh branch per sample grew the table being read. Editing one file on both sides made the land a conflict. Leaving `branch switch` inside the timer timed an operation that writes the whole tree. Each produced a plausible curve that described something other than the rebuild. What settled it was holding one variable fixed: at 500 files, `land` took 29, 20 and 30 ms with 5, 50 and 200 changesets of history, so the cost follows the files and not the history. |

## Not done, and why

| # | Not done | Reason |
|---|---|---|
| N1 | A workspace per agent by copying the tree. | The cost of a workspace is the whole tree. Sparse checkout, cheap branches and stash cover the need without a copy. |
| N2 | A local server and a SQL backend. | Both give up "one binary, one machine, no daemon". They are a separate design, not an increment. |
| N3 | A lock file for concurrent access. | It turns a working concurrent read into a failure, and a crashed process leaves it stuck with no honest way to prove it stale. WAL plus retries behaves better. |
| N4 | Guessing at another tool's behaviour to claim compatibility. | An unverified claim is worse than a missing feature. |
| N5 | Reading packfiles. | A repository whose objects are all packed has no loose objects. `git gc` gives them back. A pack reader is its own piece of work and pretending otherwise would produce a bridge that fails on the first real repository. |
| N6 | Packing several small objects into one row. | It was built and measured, and it made the repository larger: the ratio at the time went from 6.6x git to 7.7x. A pointer into a segment costs more than the seven bytes it replaces, because it is a hash, two integers and a row header. The bodies did shrink, from 98,304 bytes to a 1,959 byte segment, but `object_packed` and its index spent 172,032 bytes finding them. Recorded in docs/storage.md so the idea is not tried again without the number. The ratio on the current build is 14.9x; the packing result stands regardless of the base it was measured against. |
| N7 | Removing the per-path tables to close the storage gap. | The cost is spread over six tables that each hold a row per tracked path, and every one of them exists to make something faster: the maintained tree, the stat cache, the blame chain. Fewer rows is the fix, not smaller ones, and it costs a capability. Not done. |

## P1. A push must be inspectable before it happens

Pushing is when content leaves the machine, and a changeset carries an author string that is an address in practice. Until this existed, the only way to learn what a push disclosed was to push and look.

`remote inspect` reports the changesets, the object count, the payload size, and every author string, then separates the ones that name a person. `--check-identity` turns that into a refusal for a caller that wants a gate. A remote that cannot be asked what it already holds is reported as an estimate over the whole branch rather than guessed at, because a wrong number that looks precise is worse than an honest one.

## P2. Rewriting an author drops the signature, and says so

A signature covers the author field, because the author is part of what a reviewer is told the signer vouched for. Rewriting the author therefore invalidates it. Keeping a stale signature would turn a verified changeset into one that fails verification while still looking signed, which is worse than an unsigned changeset: the second is honest about what it proves.

## P3. A changeset id must not depend on its author

The id is derived once, when the changeset is created, and never recomputed. Everything from parent pointers to remote refs to the branch head is expressed in terms of those ids, so a rewrite that changed an id would have to rewrite the whole graph and every remote that ever received it. This is why `anonymise` can exist at all.

## P4. A replacement that is itself an identity is refused

Swapping one address for another is not anonymising. Accepting it would let somebody believe the payload is clean while it names a person exactly as before. The refusal costs nothing and prevents a false sense of safety.

## T1. A tag stores a changeset id, never the name it was given

`tag v2 main` used to store the string "main" as the changeset. The tag existed, appeared in the list, and resolved to nothing; the fault surfaced only when somebody read through it. Every revision name must be resolved before it is written, and a name that resolves to nothing must be refused rather than stored.

## T2. A tag may point at a branch, a changeset, or another tag

Following the chain at read time is what makes re-tagging honest. The walk is bounded, so a cycle written straight into the database is reported instead of hanging the process.

## T3. Blame reads its ignore list from the repository, not from local configuration

A tree-wide reformat or a symbol rename makes every line point at one changeset. True, and useless: the reader wants the change that last touched the line in substance. A list at the repository root means every clone inherits the same answer. A list that lives in local configuration gives two people two different histories of the same file, which is the one outcome a blame cannot afford.

## I1. "Nothing found" is not "nothing to find"

A review that reports no stale reference reads as a clean bill of health. That is a stronger claim than the search can support: a name built at run time or reached through an alias is invisible to a text search. Counting the places where that could happen, and saying so only when nothing was found, turns a silent gap into a stated one without hedging a definite finding.

The count comes from the same tree the search walked, so both answers describe one pass over one set of files. A count taken from a different set would answer a different question.

## G1. A check that did not run is not a check that passed

A gate reports on work nobody did when a skipped check counts as a pass. The failure is silent by construction: the run looks green, the sentence at the bottom says releasable, and the only thing wrong is that nothing was examined. Three outcomes must stay distinct, with different exit codes: passed, failed, and did not run.

## G2. A step name that matches nothing must refuse

`bench/gate.py <name>` runs one step. A mistyped name used to select nothing, print `0/0 gate steps pass`, and exit zero with the same sentence a full pass prints. Any selector must refuse a value it cannot honour, and should suggest the nearest name: a suggestion is the difference between an error somebody fixes and one they work around.

## O1. Output that was cut says where the rest is

A cap on a list leaves the reader knowing something exists and unable to see it. Every place that prints "and N more" names the command that lists all of them, and a cut line says how long it really was. The cost is a few characters; the alternative is a report nobody can act on.

## O2. Cutting a long line is opt-in, and the default is the whole line

A caller that gets a shortened line has no other place to fetch the original, so shortening by default removes information the caller cannot recover. `--max-line` exists for a caller that has decided a transcript is worth shrinking, and it states what it did.

## H1. Two layers, scored apart

A task can succeed while the tool behaved badly. Scoring only the deliverable reports that
combination as clean, and the harness benchmark literature puts the size of the gap at 31.1% of runs.
Every attempt is scored on whether the job got done and separately on five obligations about how.

A criterion must not accuse an innocent run. That is the failure that makes it useless rather than
strict: an instrument that cries wolf gets switched off. A command whose output was piped or
redirected away is not judged, and neither is a multi-line script, whose stdout belongs to no single
command inside it.

## H2. The bootstrap resamples tasks, never runs

Variance is a between-task term over the task count plus a within-task term over tasks times runs.
Adding runs shrinks only the second, and on agent work the first is usually larger. Resampling runs
would treat repeats of one easy task as independent evidence and report an interval too narrow to be
useful.

One task reports [0, 100]. A zero-width interval from a single observation is certainty nobody
earned.

## H3. A passing score needs a mutation test to mean anything

A deterministic reference answer passes every time, so the interval is zero by construction: that is
a property of the instrument, not a fact about the tool. Planting a known defect and requiring that
something catches it is what turns a score of zero defects into evidence.

Mutation found four faults in this harness that a passing run could not: a criterion reading a
truncated tail it could never parse, a criterion judging a script's concatenated output as if it
belonged to one command, and two mutants whose planted defect never reached the path they claimed.

## M1. A tool catalog is paid for on every request

Tool definitions travel with every model call, so an agent pays for the tools it never invokes. The gesta catalog is 4,426 bytes, about 1,106 estimated tokens, and it was paid again on every call because `tools/list` carried no cache hint. It now carries `ttlMs` and `cacheScope`, which the current spec requires for a client to cache at all.

The lesson is not "fewer tools". A smaller catalog that takes more steps costs more than a larger one that answers in one, and the published rebuild that cut 15 tools to 4 moved the context meter only from 1.1% to 0.8% while halving the number of calls. What matters is the cost per completed task, not the size of the list.

## M2. An argument error is not a server fault

Every tool failure used to return -32603, which tells a client the server broke. A missing `path` is the caller's mistake and JSON-RPC has -32602 for it. A client that cannot tell the two apart either retries a request that will never succeed or reports a fault that does not exist.

## M3. Tool selection is a separate thing to test

Calling a tool directly tests whether it works. It says nothing about whether anything reading twelve descriptions can tell them apart. `gesta_status` and `gesta_brief` both answer "what is going on", and a wrong choice returns a plausible answer, so the mistake never surfaces as an error.

The eval scores selection, arguments and restraint separately. Restraint matters most: a request no tool answers must be declined, because a forced fit turns "I cannot" into an action.

## S1. Blame is stored as ranges, and the author is not stored with it

A row per line repeats the changeset, the author and the timestamp for every line of every file. This project had 62,824 such rows carrying one distinct changeset, and 90% of the database was that table and its indexes. A range says the same thing once: these lines, from this changeset.

The author and the timestamp are not columns here. They have one home on `changesets`, and a reader joins for them. Duplicating a value that has one home is how a database ends up mostly repetition, and making the duplicate smaller does not compete with not writing it.

Measured: 9,236,480 bytes to 901,120 on this project, and 550,338,560 to 73,469,952 on a 10,669-file vault. Against git, 10.16x became 1.36x.
