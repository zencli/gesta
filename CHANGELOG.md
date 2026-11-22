# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every entry comes from a commit in this repository, and every version boundary is a commit that
changed the version in `Cargo.toml`. Nothing is dated by plan: a date is when the work landed. From
0.7.0 onward a version boundary is also a tag, and the release for that tag is built from the section
below.

Sections before 0.10.0 were written as freeform prose headings. They are presented here grouped under
the six standard categories, with the original text of every entry unchanged: the grouping is a
reading aid applied afterwards, not a claim about how the change was described at the time.

## [0.11.2] - 2026-11-22

### Fixed

- The installer's prebuilt path could not succeed on any machine: `install.sh` asked for
  `gesta-<platform>` where the release carries `gesta-<version>-<platform>.tar.gz`, got a 404, and fell
  through to building from source. On a machine without Rust that read as "this tool does not support
  my machine", which was wrong. Five defects, all in `try_prebuilt`: `VERSION` defaulted to `main`, a
  branch with no release assets; the asset name carried no version; the archive extension was set
  inside `platform()`, which runs in a command substitution, so the caller saw an unset variable; the
  downloaded archive was installed as if it were a binary; and the tag's leading `v` was left on the
  filename, where the release does not have it. `install.ps1` carried the same defect independently,
  asking for `gesta-<platform>.exe` and never unpacking the zip. Both now ask the API for the newest
  tag, build the name the release actually writes, unpack it, and install the binary found inside.
- `bench/installer.py` and a gate step that runs it: six static checks on both scripts, plus a live
  check that the published release carries the name the installer builds. It skips rather than fails
  when there is no network, because a runner without egress should not report a defect.

## [0.11.1] - 2026-11-22

### Fixed

- `git export` into a bare repository left `HEAD` on `refs/heads/master`. The ref was written
  correctly, every object was present, `git ls-tree` and `git show` both answered correctly, and
  `git clone` reported an empty repository. Two causes: HEAD was written before the export knew which
  branches existed, so it could only guess `main`, and it was written to `<root>/.git/HEAD`, which is
  right for a working tree and wrong for a bare repository where the git directory is `<root>`. HEAD
  is now written after the refs, from the branch that was actually written, into the git directory the
  repository really has.
- A differential test read both tools, and the missing export check is what found the above. Every
  benchmark in the tree asked whether gesta did what gesta meant to; `bench/parity.py` was named for
  parity and called git zero times. `bench/differential.py` plays one scenario twice, once with each
  tool, and reads both results back with the git binary: same paths, modes, sizes and blob ids, the
  same bytes through `git show`, a deleted file absent from both, and a clone of each compared file
  for file.
- The differential test now also exports a branch not named `main`. `main` is the branch that happens
  to match the guess `git init --bare` makes, so the original check would have passed before the fix
  and proved nothing. With the HEAD write reverted, the run is 5/7 and both clone checks fail.

## [0.11.0] - 2026-11-22

### Added

- `gesta forge push <url> <branch>` sets a branch on GitHub, GitLab or Gitea with no git binary
  installed. A push is three HTTP requests and this makes all three: read the advertisement from
  `info/refs?service=git-receive-pack`, post the command list and a pack to `git-receive-pack`, and read
  the `report-status` that comes back. Pushing to a forge used to export a bare repository to a
  temporary directory and run `git push` in it, which cost a process and a full copy of the tree on
  every push.
- Nine unit tests for the parts that decide right from wrong: an advertisement parses into refs and
  capabilities, a ref the remote does not hold answers `None` so the command carries zeroes, an HTML
  login page is reported instead of read as a repository with no branches, the capabilities land on the
  first command only, a rejected ref keeps the remote's own reason, a failed unpack is not overturned
  by an `ok` line that follows it, and a silent remote is a failure rather than a success.
- `bench/forge_push.py`, and a gate step that runs it: a store is served over HTTP, pushed to with the
  forge client, then cloned with real git and its file contents compared. A push that moves a ref
  without delivering the objects looks successful from every angle except reading the file back.
- `docs/forge.md`, which states what is verified (the local cycle, end to end) and what is not (a push
  to github.com, which needs a token and a network).

### Changed

- `docs/git-parity.md` no longer lists the forge ecosystem as something nothing here replaces. What is
  left is the other direction: bringing a repository down from a forge still uses the remote bundle
  protocol or `git clone`.
- The same document said disk is 14.9x on a tree of tiny files. That figure comes from a benchmark
  where git has run `gc --aggressive` and this tool has run `VACUUM`; measured with both sides fully
  compacted it is 5.6x.

## [0.10.1] - 2026-11-22

### Added

- A hook written for `.git/hooks` runs at the matching point. This tool's own hook takes precedence;
  when there is none, the git one runs and `hook list` says `from git` rather than `absent`. `pre-land`
  has no git equivalent, so it never reaches into `.git/hooks`.
- A git submodule survives an import and an export as a pointer. Import records a tree entry with mode
  160000 as a subrepository pinned to the child commit, and export writes it back as a gitlink.

### Changed

- Thirteen benches that asserted behaviour but were in no gate step now are, and two step names that
  had been duplicated for a while are distinct: `gate.py secrets` and `gate.py worktree` had been
  running only the first of two steps each.

### Fixed

- Import dropped every tree entry with mode 160000, so a repository with a git submodule lost the
  submodule and nothing reported it. Including the entry without also skipping it when reading blobs
  then failed the whole import with a message about an object that lives in another repository.
- `three_way` failed with `FileNotFoundError` on a runner without svn, which reads as a defect in this
  project. It now skips, naming what is missing.
- `docs/storage.md` led with 14.9x for a tree of 1000 tiny files. The ratio does not follow from the
  numbers printed beside it (856 against 103 is 8.3x), and the measurement counted the working tree on
  both sides and compared this tool mid-WAL against a packed git. Measured properly: 5.6x.
- `bench/object_format.py` asserted that a SHA-256 repository is refused, which was true before the
  tool read and wrote them. It went red the day the code improved.
- One em dash in `bench/backup_remote.py`, from a paste. The project's rule is a plain hyphen and was
  unenforced; the gate now refuses an en or em dash and any CJK character in a tracked file.

- `stash pop` put a hunk back at the top of the file when it was not the first hunk of the diff.
  `apply_hunk` used the hunk's ordinal as a line index, and the bench only ever took the first of two
  hunks near the end of a file, which is the one cell of the grid where an ordinal and a line number
  agree. A hunk in the middle of a file now goes back where it came from, and the span it replaces is
  cleared rather than inserted beside, which was growing the file by a line on every pop.
- `worktree remove` deleted a directory holding uncommitted work and exited 0. That work exists only
  on disk, so removing the directory loses the only copy with nothing to notice it. It is now refused
  unless `--force` is given, and the refusal names the count and the flag.
- The registry entry was deleted before the refusal could happen, so a refused removal left a
  directory that no later command could remove: `--force` answered "no worktree at ..." while the tree
  was still there. The lookup no longer unregisters.
- A test fixture keyed its directory by process id, so every test in `worktree` shared one directory
  and `remove_dir_all` deleted a directory another test was using. The suite failed about one run in
  three on a test unrelated to what was being changed.

## [0.10.0] - 2026-11-22

### Added

- `stash save --hunk <path>:<n>`, repeatable, takes one hunk and leaves every other edit in the file.
  The number is what `gesta diff` prints, and `diff` now prints numbered hunks: `line_diff` reported
  every change in a file as one block, so two edits a hundred lines apart came out as a single hunk
  and there was no way to name either of them.
- The dangerous half is the same as `--path`, one level finer. Taking hunk 1 and then restoring the
  file from the branch head throws hunk 2 away as well, silently. The file left behind is the original
  with only the taken hunks reverted, and the bench asserts the remaining edit is byte for byte what
  it was.
- `pop` knows the difference now. A hunk entry is stored as `path#number` with the hunk's own text,
  and pop used to write that into a file named `f.txt#1` while leaving the real file untouched, which
  made restoring look like it did nothing.
- `bench/stash_hunk.py`, eight checks, a gate step.
- `submodule add`, `init` and `update` each had `git` hardcoded. Pointing them at a child of our own
  kind gave `fatal: repository ... does not exist` on add and `fatal: not a git repository` on update,
  which are true statements about the wrong problem: the directory is a perfectly good repository of
  another kind.
- A child's store is copied whole with `VACUUM INTO` rather than replayed changeset by changeset. A
  submodule clone is the child's repository, not a summary of it, and a translation step is somewhere
  an object or a blame range can go missing.
- `submodule add` records the child's head as the pin. It used to write `commit: None` unconditionally,
  which is why `status` said `unpinned` for every submodule ever added and `update` had nothing to
  return to.
- `restore_at_changeset` writes a working tree from a changeset id. `restore_worktree` takes a branch
  name, so a pin resolved as a branch gave `no such branch: cs...`.
- `bench/submodule_own.py`, eight checks, a gate step.

## [0.9.0] - 2026-11-22

### Added

- `stash save --path <p>`, repeatable, sets aside only the named files. This is the non-interactive
  form of `git stash --patch`: a caller that knows which files it is working on says so, instead of
  picking hunks from a prompt nothing can answer.
- The bug this had to fix is severe and silent. `stash save` resets the whole tree to the branch head,
  so a path filter that narrowed the save without narrowing the reset stashes one file and then throws
  away every other edit - the work is gone, the stash does not hold it, and nothing reported an error.
  The reset now touches only what was taken.
- An empty `--path` means the whole working set, which is what `save` always meant, so both forms share
  one implementation rather than one of them drifting.
- `bench/stash_path.py`, eight checks, a gate step. The check that matters is that an unnamed file
  keeps its edits and an unnamed new file stays on disk.
- `git import` records the format it read, so a SHA-256 repository's names are 64 characters and the
  store can say so. `gesta stats` prints it, with the length spelled out, because a reader comparing
  an id against `git rev-parse` needs to know which length they are looking at.
- The HTTP capability advertisement stays `object-format=sha1`, and the attempt to make it follow the
  store was reverted after measuring it. This server puts its **own** changeset names on the wire (`cs`
  plus twelve hex characters), not git object ids, so telling a real client `sha256` made it parse those
  names as 64-character ids and answer `invalid ls-refs response`. The advertisement reports what is
  served, which is the only thing it can honestly report.
- Serving a SHA-256 repository over the protocol is therefore still not done, and the reason is now
  specific rather than a note: the names on the wire would have to be git oids, and they are not.
- `bench/protocol_format.py`, four checks, a gate step.
- The set is now `pre-changeset`, `pre-commit`, `post-commit`, `pre-land`, `post-checkout`,
  `pre-push`. Three were new; two of those turned up bugs in wiring that already claimed to work.
- **`pre-land` never ran on `branch merge`.** It was called from `land_branch`, which is the review
  queue's path, so a repository that installed the hook and merged branches directly had a guard that
  was listed as installed and did nothing. A `branch merge` is a landing: the change stops being
  private there.
- **`changeset create --json` skipped `pre-changeset` entirely.** The JSON path had its own copy of
  the create logic and the hook was only in the text path. Every agent and every bench in this
  repository uses the JSON form, so the hook was tested by nothing that could have noticed.
- `pre-push` and `post-checkout` are new, wired into `remote push`, `branch switch` and `worktree add`.
- `bench/hooks.py`, ten checks, a gate step. Each installs a real program that appends its own name to
  a file and counts the name before and after the action, because a declared hook that never runs is
  the failure this is for. Two checks assert refusals stop the action rather than warn.
- A changeset is immutable and the thing worth saying about it usually arrives afterwards: which ticket
  it closes, who reviewed it, why it was reverted. `notes add <changeset> -m <text>` attaches that text
  beside the changeset rather than inside it, so the changeset keeps its hash and every reference to it
  keeps working.
- One note per changeset, replaced rather than appended. A list would need ordering and merging nobody
  asked for, and the second note is almost always a correction of the first.
- A note against a changeset that does not exist is refused rather than stored: a row nothing can read
  back hides the typo in the id instead of reporting it.
- `bench/notes.py`, eight checks, a gate step. The check that matters most asserts the changeset is
  byte for byte what it was before the note was attached.
- Two branches cannot be checked out in one directory at once, and the alternative - clone the
  repository again - duplicates every object and then has to be kept in step by hand. `worktree add
  <path> <branch>` makes a second working tree whose `.gesta` is a **file** naming the first one's
  store, the way git marks a linked worktree with a `.git` file. One database, two trees, so a commit
  in either is immediately visible in the other.
- A working tree's branch is a property of the tree, not of the repository. `current_branch` read one
  row out of the store's shared state, which can only describe one tree: before this change a commit
  made in a linked worktree landed on **`main`** instead of on the branch the worktree was created
  for. Each tree now records its branch in `.gesta-branch`, and `open_repo` installs that answer for
  the rest of the process. A repository with one tree never writes the file, so nothing changes for it.
- `worktree remove` deletes the files the tree wrote and nothing else. The first version used
  `changeset_files`, which holds what a changeset *changed*, so it would have deleted only the files
  the last commit touched and left the rest of the checkout behind; it walks the branch tree now. A
  file the store never wrote stays, and the directory itself only goes when it is empty.
- The store is never touched by `remove`. The branch, its changesets and every object remain.
- `bench/worktree.py`, seven checks, a gate step.
- `gesta bisect run <command>` answers every question by running the command, adopting git's exit
  code convention: 0 is good, any other code is bad, and **125 means skip**. The skip case is the
  one that matters - a changeset whose build does not compile must be dropped from the range without
  being reported as an answer, or the search names a commit that was never tested.
- A command that cannot be started at all is an error, not a verdict. `sh: not found` is not evidence
  that a changeset is bad.
- Each verdict is printed as it is decided, because the useful part of an automated search is which
  changeset first failed, not only the final name.
- `bench/bisect_run.py`, six checks, a gate step, including the skip case and the assertion that the
  working tree is back at the branch head afterwards.
- The previous entry refused the format by name. That was the honest half of not supporting it, and
  it is now replaced by support: `git import` reads a `--object-format=sha256` repository and
  `git export` writes one, and a round trip reproduces the original commit hash exactly.
- The length of a name is the whole problem, and it is not in one place. `HashKind` travels on `Repo`,
  and `parse_tree`/`build_tree` take it as an argument because the raw hash is 20 bytes under SHA-1
  and 32 under SHA-256 and the entry does not record which. A parser that assumes 20 reads the first
  twelve bytes of a SHA-256 hash as the next entry's mode, which surfaced as "tree entry has no mode
  separator" on a tree git had just written correctly - an error naming the wrong thing.
- `object_hash_with` computes the name under a given format, and `Repo::object_hash` picks the
  repository's. The free `object_hash` stays as the SHA-1 default for callers with no repository in
  hand, so a test fixture keeps its meaning.
- The protocol still advertises `object-format=sha1`. Serving a SHA-256 repository over HTTP needs
  the capability negotiated per repository and the ref advertisement to carry 64-character names, and
  that is not done here.
- `bench/sha256_repo.py`, nine checks, a gate step. It makes a real repository with real git, imports
  it, exports into a fresh SHA-256 repository, and lets `git fsck` and `git rev-parse` judge the
  result. A SHA-1 round trip runs beside it, because every change here is on a shared path.
- `gesta serve` answers the git-lfs batch API and the object route, so a tree holding a pointer can
  be resolved by a client that speaks LFS. Both sit under the same prefix as the repository, so one
  auth decision covers them.
- The `href` in a batch response is a path rather than an absolute URL. The request carries no host
  and no scheme, and inventing one produces a link that is wrong behind a proxy or on a non-default
  port; a client resolves a relative href against the endpoint it called.
- A missing object is reported inside a `200` batch response, not as an HTTP error, because one
  absent oid must not fail the other ninety-nine. An `upload` for an object the store already holds
  gets no actions block, which is a different answer from missing: a client reading "missing" for an
  upload retries forever.
- `gesta git export` writes the three-line pointer instead of the payload when the store kept the
  object in the sidecar, and `gesta git import` resolves it back. The export path used the text
  reader, so a binary export failed with the store's own hash as the error message - one check in
  `bench/lfs.py` exists for that alone.
- `bench/lfs.py`, ten checks, a gate step. It exports a 5 MB payload to a real git repository, reads
  the blob back through `git cat-file` to prove it is text, then drives the batch API against a real
  server and compares what the download link serves against what was committed.
- `.gitattributes` carries `large` and `small` beside its line-ending rules, and a path marked
  `large` goes to the sidecar whatever its size. `-large` sends it back. Text needs this as much as
  a binary does: a 20 MB generated JSON belongs outside the database for the same reason.
- Before this, the only trigger was the 4 MB threshold, which is a guess about a file the writer
  already knows the shape of.
- `backup-remote push|list|pull|prune` uploads an existing store backup to S3-compatible storage.
  The archive is sealed with ChaCha20-Poly1305 before it leaves the process; the key is
  HMAC-SHA256 over a random salt, using the primitive `support/secrets.rs` already had.
- The passphrase comes from the environment and never from the URL or from inside the sealed file,
  so a bucket that leaks gives away nothing but bytes.
- Prune keeps a count rather than an age: a schedule that is forgotten still leaves the old files
  behind, and a schedule that runs leaves ten copies is enough.
- Name resolution is milliseconds, not seconds. Five pushes inside one second wrote the same key and
  the bucket held one backup, so prune had nothing to remove and the test found it.
- A request carrying `Git-Protocol: version=2` is answered with the capability list instead of
  refs, and its POST is read as `command=ls-refs` or `command=fetch`.
- `ls-refs` honours `symrefs`, `unborn` and `ref-prefix`, and names HEAD first.
- A fetch that named haves and did not say `done` gets an `acknowledgments` section; one that
  said `done` gets `packfile`.
- `read_pkt_lines` now walks past `0001` and `0002`, the v2 delimiter and response-end.
- `bench/protocol_v2.py` forces a real client to v2 and clones and fetches.
- The server advertises `multi_ack_detailed` and answers it: an `ACK <sha> common` per commit
  the client has, `ACK <sha> ready` for the last one, then a final `ACK <sha>` before the pack.
  A client that did not negotiate it gets the `NAK` it expects.
- `bench/multi_ack.py` drives a real clone and two fetches and reads git's own packet trace to
  confirm all three ACK forms arrived.
- `gesta key rotate` archives the key in use under a name that names it, then makes a new one.
  Nothing is re-signed: a signature names the public key it was made with.
- `gesta key generate` records the key as active, so a repository that never rotates still
  answers which keys were ours.
- `gesta key list` shows every key and which one is active.
- `gesta verify` names a public key this repository has never recorded.
- `bench/key_rotate.py` checks that an old signature still verifies after a rotation.
- `audit.log` now holds commits, sign-offs, key rotations and pushes, not only what the server
  decided about a request. The same lines go to a configured collector.
- `bench/audit_local.py` does each action over a real socket and reads the line back.
- `gesta submodule add <path> <url>` writes a `.gitmodules` entry; `init` clones missing ones at
  their pin and `update` moves them to it. A pin may be the short id from `git log`.
- `submodule status` reports clean, dirty, behind or missing for each one.
- The parent's walk skips every submodule checkout. Without that, the child's files were recorded
  as the parent's and a change in the child looked like a change here.
- `bench/submodule.py` checks all of it, including that the child's files never reach the parent.
- `gesta remote token rotate <name>` replaces the secret and keeps the role, path scope, branch scope
  and name. The old secret is refused from the next request, because the server re-reads the auth file
  on every request.
- `token add` accepts the `approve` role.
- `bench/token_rotate.py` presents both secrets to a running server instead of comparing the file, and
  the gate runs it.
- `gesta branch protect <name>` makes a push to that branch need a sign-off, and
  `gesta changeset approve <id> --by <reviewer>` is the sign-off. Both live in the store, so a clone
  carries the rule.
- A new `approve` role may read and sign off and may not write. A reviewer holding a write token is
  the failure this prevents.
- An approval from the credential that is pushing does not count.
- A refused push leaves the branch where it was. The import used to move it before the gate ran, so a
  rejected push had landed.
- A server with no tokens refuses a push to a protected branch and says so, because an anonymous
  pusher cannot be told apart from a reviewer.
- `bench/branch_protect.py` drives a real git client for six checks; the gate runs it.

### Changed

- `gesta git import` now reads `objectformat` from the repository's config and refuses anything other
  than `sha1`, naming the format and the conversion. Before this, pointing the importer at a SHA-256
  repository reported a missing object, which is the same message a corrupted store produces - so a
  reader went looking for damage instead of a format mismatch.
- This is a refusal, not support. Reading a SHA-256 repository means 64-character oids through the
  store, a different hash length in every pack trailer, and `object-format` negotiated in the
  protocol. There are 24 places in this code that assume a 40-character oid, and each one is a silent
  wrong answer rather than a compile error, which is why the work has not been done in one commit.
- A repository with no `objectformat` line keeps git's default and opens, so a hand-made bare
  repository is not caught by the guard; `bench/object_format.py` checks that beside the refusal.
- `bench/object_format.py`, seven checks, a gate step. The fixture is made by real git with
  `--object-format=sha256`, not by writing a config by hand.
- 1,000 files of seven bytes cost 856 KB, which is 14.9x git's 103 KB. The figure in the docs was
  6.6x and had been wrong for months, because nobody re-ran it after blame moved from one row per
  line to ranges.
- The cost is not the object store - 1,000 identical bodies dedup to one row. It is five tables of
  one row per path at about a hundred bytes each.
- Storing hashes as 32-byte blobs instead of 64-character hex measures 11% and is deliberately not
  done: the value is a `String` through the whole storage layer and the change reaches about fifty
  call sites, which is not worth 11% on a tree already at 18x its floor.
- `bench/tiny_files.py`, `bench/hash_size.py`, and a `storage-cost` gate step that fails above 18x.
- The walk collects paths, then stats them over four threads in contiguous slices. At 100,003
  files the stat phase went from 78 ms to 24 ms and a whole `status` from 205.6 ms to 183.4 ms,
  which against `git status` is 3.63x to 3.19x.
- Below 512 files the walk keeps its serial loop: the thread spawn costs more than the stats.
- `bench/stat_parallel.rs` is the measurement that chose four threads. Two left the filesystem
  idle, eight spent more on handoff, and a locked queue measured 31 ms against 24.
- An object of 4 MB or more is written to `.gesta/objects/<aa>/<rest>` and its row keeps only
  the name and size. An inline blob that size is copied by every backup, VACUUM and page read.
- Both readers ask the sidecar first, so nothing above the store knows which shape an object
  has. `fsck` reports a sidecar object whose file is missing, which is the one inconsistency
  the change adds.
- `gesta show` writes bytes rather than text. It went through the text reader, so showing any
  binary file failed with a message about UTF-8, which the sidecar test found.
- `bench/sidecar.py` checks the whole path with a 4.5 MB file.

### Fixed

- A file renamed with no edit had every line credited to the commit that moved it. Two causes, and
  fixing either alone left it broken: `commit_one` deleted the old path's blame rows because from
  inside the loop a rename and a deletion look identical, and the new path has exactly one version so
  there was nothing to diff against even when the rows survived.
- A rename is now told apart from a deletion by asking whether this same changeset added the same
  content under another path. A real deletion still drops its rows, so `blame_ranges` does not grow a
  permanent entry for every path that ever existed - the two decisions look the same from inside the
  loop and pulling one way breaks the other.
- The rows to inherit are found through `rename_history`, the same walk `gesta history` prints, so
  the two commands cannot disagree about what counts as a rename. That walk returns this path first
  and the earlier names after, so the oldest is the last entry; taking the first returns the path
  itself and puts the reader straight back on the one-version branch this exists to avoid.
- `gesta history` was already correct, which is why a unit test of the store's bookkeeping missed
  this: the chain printed `main.py <- app.py` while blame on `main.py` named the rename. The check
  that catches it drives the binary.
- `bench/blame_rename.py`, six checks, a gate step. It also asserts that a deleted file keeps no
  blame rows, because that is the half a careless fix breaks.
- `merge_base` returned a single best common ancestor, which is right for an ordinary fork and
  wrong for a criss-cross: two branches that each merged the other, then diverged again. There are
  two bases, neither reaches the other, and picking one silently discards what the other carried.
  The merge reported success and a change was gone.
- `merge_bases` returns the whole set, and the merge recurses - two bases merge into a virtual base,
  then ours and theirs merge against that. `merge_base` stays as the single-answer wrapper for
  callers that can only diff against one, so the truncation is visible at each call site.
- The first fixture written for this was not a criss-cross, and the test said so: it returned
  `["a1", "b1"]` where `["m1a", "m1b"]` was expected, and those answers were correct for the graph
  that had been built. Making each merge a parent of both new tips is what turns the shape into one.
- The detector paired deletions with additions by a line-similarity ratio against 0.5 - a heuristic,
  and the wrong tool for the case that matters most: a file renamed with no edit. Equal content
  hashes are the same file with no judgement needed, so the exact pass now runs first and takes the
  pair out of the similarity pass.
- A rename plus a heavy rewrite stays below the threshold and is reported as a delete and an add.
  That looked like a bug until git was run on the same shape: `log --follow` reports one commit, not
  two, because git's default is 50% and this case is under it. The test now pins both directions,
  including the one where finding nothing is correct.
- `bench/renames.py`, five checks, a gate step.
- `show --json` printed the file's bytes and called it JSON. A caller piping into a parser got
  the file's contents where an object should be, and had no way to tell.
- The body is `content` when it is UTF-8, and `content_base64` when it is not, with `binary`
  saying which. A body that is not UTF-8 cannot be a JSON string at all.
- `--json` now reaches `cmd_show`. The flag was declared and dropped on the way, so it read as
  an option that did nothing rather than an option that failed.
- `show` without `--json` still writes the bytes untouched, which is what a pipe into a file
  expects, and the check pins that too.
- `git import` now builds the blame chain for every path. It wrote `file_versions` and never
  `blame_ranges`, so `blame` answered `never committed` for every file an import brought in
  while `history` found versions for the same path.
- The chain is walked oldest first. Comparing only the newest version against nothing, which is
  what the per-commit path does, attributed a whole file to the last changeset that touched it.
- `resolve_rev` accepts `HEAD`, a git commit id or its abbreviation, and an unambiguous prefix
  of a changeset id. A `vN` label resolves with a path and is refused with the form to use.
- `history` prints the changeset id in its second column instead of the file version's content
  hash, which looked like a commit and resolved to nothing.
- `bench/revisions.py` takes each spelling from real output and feeds it to another command.
- `.gitattributes` support for line endings. `text` normalises to LF in the object, `eol=lf` and
  `eol=crlf` decide what the working tree gets, and `-text` or `binary` stores bytes unchanged. Read
  from `.gitattributes` and from `.gesta/attributes`, with the last matching rule winning.
- `ignore attr <path> [--json]` says which rule decides how a path is stored and checked out.
- `diff --to <branch>` without `--from` compares against that revision's parent instead of the empty
  tree. `--from ""` still asks for the whole tree.
- `review --json` prints one parseable document instead of text.
- A push that deletes a branch deletes it. The empty-pack path used to answer `ok` without doing
  anything, so git printed `- [deleted]` for a branch the server still had.
- `changeset create --path` limits a snapshot to named paths, so two agents in one checkout do not
  record each other's work under their own name.
- Measured to 100,000 files: the write stays flat at 13 ms while `git add` plus commit goes from
  15.6 to 100.2 ms. The scan is slower than `git status` at that size, so the whole path is slower
  here, which the documentation says plainly.
- `status` at 100,000 files went from 388.0 to 299.6 ms by removing a sort that no caller read,
  which is 5.23x to 4.04x against `git status`.
- `bench/many_writers.py` measures 100 concurrent writers; every one lands a commit and fsck is
  clean.
- A commit no longer rewrites the file's whole blame table. On a 10,000 commit history the write
  went from 47.1 to 29.4 ms, and the growth over 100x history from 3.47x to 2.08x.
- `bench/long_history.py` measures a commit against the length of the history.
- Every authenticated request and every refusal is one JSON line in `.gesta/audit.log`, read with
  `gesta remote token audit --refused`. A log that held only successes never showed what was tried.
- A token can be scoped to named branches with `--branches`, so a write token for one branch
  cannot rewrite another. A refused push leaves no branch behind.
- A token removed while the server runs stops working on the next request, without a restart.
- `gesta backup`, `gesta verify-backup` and `gesta restore-repo` copy and restore the whole
  store: every branch, tag and blame range, taken through SQLite's backup API so a repository being
  written still produces a consistent file. Restoring verifies first and keeps the old store until
  the new one opens.
- `gesta serve` speaks the git upload-pack and receive-pack protocols, so `git clone http://host/git/repo` works with a
  plain git client and no gesta binary, and a plain git client can push. Full pack on the way out,
- `git export` writes the timezone the commit was imported with, so every commit keeps its git name
  through an import and export. It wrote `+0000` for everything, which gave every exported commit a
  different id: 0 of 97 survived on this project's history, 97 of 97 after the fix.
- `bench/harness/compare.py` measures three repository shapes instead of one, and reports each shape
  before the pooled figure. Measured: 0.75x on a small tree, 1.00x on deep paths, 0.67x where
  deletions are involved. One shape was one sample, and the three do not agree.
- `gc` groups loose text objects into shared compressed streams, 50 to a group and sorted by size.
  Measured on 11,225 files: 69.2 MB before, 51.4 MB after. Each object keeps its own row, hash and
  identity, so `fsck`, `gc` and a partial read stay simple; only the body moves.
- `git import` writes `file_versions`, so blame, `grep --history` and the history secret scan work
  on an imported repository. It wrote only `changeset_files` before, which left every per-path
  question over time unanswerable and silent about it.
- Secret scanning at commit time. A changeset whose content looks like a credential is refused and
  nothing is written; `secret scan [--history]` answers the same question for the tree and for every
  stored version. Rules are a pattern plus a per-rule entropy floor, findings carry a fingerprint that
  does not move with the line number, and `.gesta/secret-allowlist` covers the fixtures a project
  needs to commit.
- `bench/scale.py` measures the scan and the write apart, because one figure for both hid a flat
  write behind a growing scan. Measured: the write costs 1.28x more in a repository 16x larger.
- `prune_cache` asks whether any path disappeared before reading the whole table back.
- Storage measured against a packed git for the first time: 72.8 MB against 37.4 MB on 11,301 files,
  so 1.95x rather than the 10.2x the parity document had been carrying. That figure predated the blame
  rewrite; the table is now 1.2% of the database and objects are 82%.
- `bench/disk.py` measures the two stores with both at their best, using `git gc --no-detach` because
  a plain `git gc` detaches and returns before it has packed anything.
- The remaining gap is cross-file compression, measured at 1.50x by joining the objects into one
  stream. Raising the zlib level changes nothing (47.3 MB at level 6, 47.2 at level 9).
- `git fetch` now honours the client's `have` list: every object those commits reach is left out of
  the pack. On a sixty file tree with one file edited, a full fetch is 7662 bytes and a fetch carrying
  a `have` is 1739.
- `read_pkt_lines` stopped at the first flush, which dropped every `have` a fetch sends after its
  `want`s. A server that read the list therefore behaved exactly like one that ignored it.
- `bench/fetch_negotiate.py` measures both requests on the wire against a real server, and compares
  the edit against what `git pack-objects` charges for the same change. The gate runs it.

## [0.8.1] - 2026-11-20

### Added

- `gesta serve` posts each audit line to a collector configured in `.gesta/audit-url`, with the bearer
  token in `.gesta/audit-token` so the URL never carries a credential.
- Delivery is queued and drained on a background thread: a dead collector cannot slow a request, and a
  full queue is counted rather than grown.
- `bench/audit_e2e.py` runs the real server against a real collector.
- The release workflow builds six targets: linux x86_64 in glibc and musl, linux aarch64, macos on Intel
  and Apple silicon, and windows x86_64. Each archive is attached to the release for its tag.
- Every target runs its own binary before packaging: `--version`, `init`, `status`. A build that
  compiles and cannot start is what that step catches, and the first Windows build needed a DLL beside
  the binary with no compiler error to say so.
- The publish job reads the asset list back from the API and fails when fewer than six arrived.

## [0.7.0] - 2026-11-20

### Added

- `gesta mount X:` presents a branch through WinFSP without a checkout. Read-only, because a write to a
  branch that is not checked out is a merge policy and not a filesystem call.
- The pack this server writes carries git deltas. On this project's history: 6633 to 1700 KiB, 6.65x
  git's pack to 1.70x, with git fsck clean and every object rebuilding byte for byte.
- Delta chains are capped at 50, which is git's own limit. A cap of 8 was tried and made the pack
  larger, because git's own pack of this history reaches 34 levels.
- The first build of this project on Windows. The Windows SDK and LLVM were missing, and Smart App
  Control blocked every cargo build script until it was turned off.
- `status` at 100000 files went from 388 to 274 ms across this release, and the comparison with git is
  now produced by one script that runs both tools rather than assembled from separate runs.

## [0.6.0] - 2026-11-18

### Added

- `gesta mount X:` presents a branch through WinFSP without a checkout. Read-only, because a write to
  a branch that is not checked out is a merge policy and not a filesystem call.
- The pack this server writes now carries git deltas. Measured on this project's history: 6633 to 1700
  KiB, from 6.65x git's pack to 1.70x, with `git fsck` clean and every object rebuilding byte for byte.
- Delta chains are capped at 50, which is git's own limit. A cap of 8 was tried first and made the pack
  larger, because git's own pack of this history reaches 34 levels deep.
- The first build of the project on Windows: the Windows SDK and LLVM were missing, and Smart App
  Control blocked every cargo build script until it was turned off.
- Every attempt now carries a cost: bytes read, turns, and cost per pass. A pass rate says the work
  can be done and says nothing about what it cost, and the published harness benchmark that held
  the model fixed found cost moving 17.5x across harnesses while quality stayed within 17 points.
- Setup and checks are not charged, because they are the harness's work rather than the tool's.
- Division is by passes, not attempts. A run with no passes reports the absence of a figure rather
  than zero, which would read as free.
- `review --json` used to print text: the flag was accepted and ignored. It carries the same answer
  as data now, with each step's risk band, score, reasons, files and stale references.
- `bench/harness/compare.py` prices gesta and git on the same six questions, in the form each tool
  has for that answer. Measured: gesta answers all six at 0.92x git's cost per answer.
- Eight tests require the cost model to separate a quiet tool from a chatty one, and to refuse a
  ratio against zero passes.
- Every task carries a reference answer, because a check that has never been passed cannot be told
  apart from a broken one.
- `--selftest` runs each setup with no work done and requires that not every check passes. It found
  three tasks that performed their own goal inside the check, and one that restored the state it
  started in and so could not tell work from no work.
- `--mutate` plants four kinds of defect across the task set, nineteen plantings in all, and
  requires that every one is caught by a task check or a criterion. It found four faults a passing
  run could not: a criterion reading a truncated tail it could never parse, a criterion judging a
  script's concatenated output as if it belonged to one command, and two plantings whose defect
  never reached the path they claimed to test.
- Attempts end as passed, failed, timed out, or broken, with the last two as separate outcomes. A
  run stopped by a clock says nothing about the work.
- Commands run in a scrubbed environment: an allowlist, an explicit list, and a secret-shaped name
  dropped even when listed. Credentials are on neither list.
- Every attempt is scored on the deliverable and separately on five obligations: quiet error stream
  on success, a refusal that explains itself, no database path or SQL error reaching the reader, no
  panic, and `--json` a parser can read.
- `pass@k` and `pass^k` are reported together, with bootstrap intervals, because capability with
  retries and reliability without them are different questions.
- The bootstrap resamples tasks and never runs. One task reports `[0, 100]` rather than certainty
  nobody earned.
- `--selftest` and `--mutate` are gate steps, so a passing score cannot be produced by an instrument
  that sees nothing.
- `tools/list` carries `ttlMs` and `cacheScope`. The catalog is 4,426 bytes, about 1,106 estimated
  tokens, and a client following the spec could not cache it, so it was paid on every model call.
- `server/discover` reports the protocol version, the server identity, and instructions that say
  which tool to read first and how it differs from its nearest neighbour.
- An argument error is `-32602` instead of `-32603`. A missing `path` is the caller's mistake, and a
  client that cannot tell that from a server fault either retries forever or reports a fault that
  does not exist.
- `bench/harness/mcp_eval.py` scores what calling a tool directly cannot: thirteen requests, each
  naming the tool that answers it and the arguments it implies, with selection, argument accuracy,
  per-tool precision and recall, and restraint scored apart. A request no tool answers must be
  declined, because a forced fit turns "I cannot" into an action.
- Every capped list names the command that lists everything: `diff --json` for skipped binaries,
  `review --json` for stale references, `remote inspect --json` for author strings.
- `grep --max-line` cuts a long line and states its real length. Cutting is opt-in, because a caller
  that receives a shortened line has no other place to fetch the original.
- A review that finds no stale reference reports how many files could hide one, and the count of
  places that build a name at run time or forward it through another name. Silence in that case
  made a clean result and a blind result look identical.
- `remote inspect` reports what a push would carry before it carries it: changesets, bytes, and
  every author string, with the ones naming a person counted apart. `--check-identity` makes that a
  refusal.
- `anonymise` replaces the author on changesets that already exist. A signature covers the author,
  so the rewrite drops it and says how many; a replacement that is itself an address is refused,
  because swapping one identity for another is not anonymising.
- `grep --history` searches every recorded version, `--regex` and `--word` search by shape.
- `brief` answers where a session stands in one call, and `brief --fix` does the safe half of what
  it reports.
- Subrepositories, with a path that points at another repository.
- A JSON schema version on every document, because a machine parses it and a person does not.
- Object storage over S3 with SigV4, needing no server, with credentials sealed and never in
  `argv` or a URL.
- A shallow clone takes only the newest changesets and can be extended later.
- `land` refuses when the target moved since it was read.
- Structured errors: ten codes, an exit status that carries the same distinction, and one JSON
  document per invocation.
- Git interoperability: import and export with matching commit ids, verified against real git.
- `gc`, and the true storage numbers in `docs/storage.md`.
- Packing small objects was measured, made things worse, and was removed. The numbers are recorded.
- Binary files are stored raw rather than refused.
- Non-ASCII paths no longer panic.
- `fsck --worktree` checks the disk as well as the database.
- `impact` finds callers a change breaks outside its own diff.
- A landing can be refused by a hook, with both branches left untouched.

### Fixed

- A push that deletes a branch deletes it. The empty-pack path answered `ok` for every command
  without doing anything, so git printed `- [deleted] throwaway` for a branch the server still had.
- `delete-refs` is advertised, a zero new id is a deletion, `old` is checked before anything is
  removed, a token scoped to other branches is refused, and the last branch cannot be deleted
  because a repository with no branch refuses every command that needs one.
- `tag` resolves its target before storing it. `tag v2 main` used to store the string `main`, and
  the tag existed, appeared in the list, and resolved to nothing.
- A tag may point at a branch, a changeset, or another tag, with a bounded walk so a cycle is
  reported rather than hanging. `tag --list` no longer demands a name it does not use.
- `blame --ignore-rev` names a revision whose lines should be attributed to the change before it,
  and `.gesta-blame-ignore-revs` at the repository root is read without being asked.
- A hook that changed after installation is refused rather than executed, and `hook status` shows
  both hashes. `pre-land` runs before a branch merges into a shared target, which is when a change
  stops being private and the last point where a failing suite costs one person instead of
  everybody.
- `review --from <branch>` uses the merge base rather than the other branch's head. Reviewing a
  feature against a main that had moved one commit used to list every changeset main already had.
- Three outcomes with three exit codes: passed, failed, and did not run. A skipped check was
  recorded as a pass, and a mistyped step name selected nothing and printed the same sentence a full
  pass prints. `osv` looked for its scanner at a one-off path and reported "did not run" on a
  machine that had it installed.
- A delta could not decode a line that was blank.
- Undo to an empty branch left every file on disk.
- Two paths differing only by a backslash collided on Unix.
- A three-way merge duplicated a line the other side had already written.
- A stash with a deletion reported there was nothing to save.
- An installer left a partial file behind when a download failed.
- A `winget` suggestion that changes nothing was replaced with the command that does.

## [0.5.0] - 2026-11-17

### Added

- Deltas are sent on the wire, so a bundle follows the size of the change rather than the size of
  the history.
- Sparse checkout limits the working tree to a set of paths, with the scope applied to both sides of
  the comparison so a file in the branch tree that was never written is not read as deleted.
- `branch conflicts` reports a conflict as data, per region, with both sides and context.
- TLS, behind a feature so a build without it stays small.
- An S3 remote that needs no server, with SigV4 signing.
- Remote credentials stored sealed, and a delta push no longer erases a bucket.
- A branch list read from a bucket, and the hosting comparison in `docs/hosting.md`.

### Changed

- `undo` is reversible and keeps the working tree honest.
- `land` uses a compare-and-swap, and the risk score learns from reverts.
- `review` shows a stack step by step with a reason for every risk point.
- `diff --from` and `diff --to` compare any two revisions.
- A pending snapshot that no longer matches the disk is reported.
- A name that is not found reports what does exist.
- The committed tree is read from a maintained table instead of rebuilt, so commit cost follows the
  size of the change rather than the size of the repository.
- The ignore check allocates nothing per pattern.

## [0.4.0] - 2026-11-17

### Added

- Access control for the HTTP remote: tokens, and an access log.
- MCP server over stdio, so an agent can act over the protocol.
- JSON output on every read command.
- A stat cache, and a git comparison.
- A release gate, which closed two dependency advisories on its first run.

## [0.3.1] - 2026-11-17

### Added

- An HTTP remote, so two machines can share a repository without a shared filesystem.

## [0.3.0] - 2026-11-17

### Added

- Three-way merge, undo, `fsck`, and bundles.
- Usable as primary source control for one developer.

## [0.2.0] - 2026-11-17

### Added

- First version. Source control for AI agents: changesets, branches, a working tree, and a store.
- Named ZenForge at this point. Renamed to gesta later in the same day, and the MCP tool prefix
  accepts the old name so a client configured before the rename keeps working.
