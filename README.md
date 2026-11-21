# gesta

<img src="assets/gesta.svg" alt="gesta" width="112" align="right">

[![repo](https://img.shields.io/badge/github-zencli%2Fgesta-181717?logo=github)](https://github.com/zencli/gesta)

Source control for AI agents. Changesets live in SQLite, objects are content addressed and compressed, blame and history are derived on commit. One Rust binary, no daemon.

## Why

Git assumes a human working copy. You clone everything, stage, commit, push. An agent reads one file, writes one file, and does that thousands of times a day, so a full clone is wasted work and a full round trip per edit is wasted time.

gesta keeps the repository server side and answers questions about it directly. An agent can commit without ever holding the whole tree.

## Install

One command. It downloads a prebuilt binary where one exists for this machine and builds from
source otherwise, then prints the next steps ([docs/install.md](docs/install.md)).

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/zencli/gesta/main/install.sh | sh
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/zencli/gesta/main/install.ps1 | iex
```

A machine with no Rust still installs, because a prebuilt binary is tried first. Without one,
the installer builds from source and says how to get Rust in one command.

From source, without the installer:

```sh
cargo install --git https://github.com/zencli/gesta.git
# or, in a checkout: cargo build --release
```

Dependencies: rusqlite (bundled SQLite), clap, sha2, hex, flate2, rustls, ring. No runtime, no
service, no daemon.

## Quick start

```bash
cd my-project
gesta init
gesta changeset create "Add feature" --author agent
gesta changeset commit cs1a2b3c4
gesta log
gesta blame src/main.rs
```

## Commands

| Command | Description |
|---|---|
| `init [--bare]` | Create `.gesta/` with the SQLite database |
| `status` | Branch, HEAD, pending changesets, added/modified/deleted files |
| `diff [--changeset <id>] [--name-only]` | Working tree diff, or one changeset's diff |
| `log [--limit N] [--oneline]` | Changesets, newest first |
| `changeset create <msg> --author <a> [--session S] [--intent I]` | Snapshot the tree, recording which session made the change and why |
| `changeset commit <id>` | Commit one changeset, move the branch head |
| `changeset commit-all` | Commit every pending changeset in order |
| `changeset apply <id>` | Commit this and all earlier pending changesets |
| `changeset diff <id>` | Line diff for one changeset |
| `changeset list` | Pending changesets |
| `blame <file>` | Which changeset introduced each line |
| `history <file>` | Version chain with hash, time, author |
| `show [<rev>:]<file>` | File content at a changeset or branch |
| `ls [<rev>] [--path P] [--long] [--json]` | Every path the revision holds |
| `checkout <file \| .> [--changeset <id>]` | Materialise files from history |
| `restore <file \| .>` | Discard uncommitted changes |
| `sub add <path> <remote>` | Record a path as a subrepository |
| `sub list [--json]` | Pointers, what each is pinned to, and whether the checkout is present |
| `sub pin <path>` | Pin the child at its current head |
| `sub remove <path>` | Stop tracking the path as a subrepository |
| `stash save [--message M]` | Save the working tree aside and reset to the head |
| `stash list` | Saved stashes, newest first |
| `stash pop [--id N]` | Write a stash back, refusing over local changes |
| `stash drop [--id N]` | Delete a stash without applying it |
| `git import <path>` | Read a git repository into this one, no git binary needed |
| `git export <path>` | Write a git repository that git reads, same commit ids |
| `git push <remote>` | Send branches to any git remote through the git binary |
| `git pull <remote>` | Fetch branches from a git remote and import what is new |
| `gc` | Remove unreachable objects and vacuum the database |
| `brief [--json] [--fix] [--stash]` | Where the session stands, and `--fix` does the safe part |
| `grep <text> [--history] [--regex]` | Find text in the working tree, or in every recorded version |
| `hook list` | Which hooks are installed |
| `hook install <name> [--file F]` | Write a hook, reading stdin when no file is given |
| `hook remove <name>` | Remove a hook |
| `hook status [--json]` | Whether each hook still matches what was installed |
| `tag <name> [--changeset R]`, `tag --list` | Tag a branch, changeset or another tag |
| `remote inspect [--check-identity]` | What a push would carry, without carrying it |
| `anonymise [--author A] [--to S] [--dry-run]` | Replace the author on existing changesets |
| `blame <file> [--ignore-rev R]` | Which changeset introduced each line, past the revisions you name |
| `rebase <onto> [--branch B]` | Replay a branch's changesets onto another point |
| `rebase --continue` | Finish a stopped rebase using what is on disk |
| `rebase --abort` | Put the branch back where the rebase started |
| `diff --from A --to B` | Compare two revisions, for reviewing a step |
| `review [--from A] [--min-risk B]` | Read a stack step by step, with a risk read on each |
| `land <branch> --onto main --expect H` | Merge only if the target still has head H |
| `cherry-pick <id> [--onto B]` | Apply one changeset's change onto a branch |
| `bisect start <good> <bad>` | Begin a search for the changeset that introduced a fault |
| `bisect good \| bad` | Answer about the changeset under test |
| `bisect status` | Remaining candidates and questions asked |
| `bisect cancel` | Abandon the search and restore the working tree |
| `undo` | Undo the last operation |
| `ops [--limit N]` | Operation log |
| `tag <name> [--list]` | Named pointer to a changeset |
| `branch create/list/switch/delete/merge/info` | Branch operations |
| `remote add/list/push/pull/clone` | Remote sync over a path, `http://`, `https://` or `s3://` |
| `remote clone --depth N` | Take only the newest N changesets, then restore the full working tree |
| `remote deepen --depth N \| --unshallow` | Take more history, or all of it, without moving the head |
| `remote ls` | List the branches a bucket holds |
| `remote token add <role> <name> [--hashed]` | Generate an access token for serving |
| `remote token list` | Show configured tokens, last four characters only |
| `serve [--bind ADDR] [--port N] [--tls <cert\|self-signed>] [--key K]` | Serve this repository over HTTP or HTTPS |
| `mcp` | MCP server on stdio, exposing the repository as tools |
| `branch conflicts <source>` | Report merge conflicts as JSON without touching anything |
| `sparse set/list/clear` | Limit the working tree to a set of paths |
| `fsck [--prune] [--worktree]` | Verify integrity, optionally drop orphan objects; `--worktree` also compares the files on disk with the head |
| `stats` | Object counts and compression ratio |
| `ignore add/list/remove` | Ignore patterns |

### A versioned contract

Every JSON document carries `"schema": "gesta@1"`, and so does every MCP tool result. A parser
that meets a version it does not know should refuse it rather than guess at field names
([docs/json-contract.md](docs/json-contract.md)).

`log --json` and `changeset list --json` stay bare arrays, because a list cannot carry a key and
wrapping it would break the pipes people write. `--envelope` asks for the versioned object.

### Starting a session

```sh
$ gesta brief
branch:   main
head:     cs6455603ddbe5
next:     commit-work
changed:  1 file(s) differ from the head

the working tree differs from the head: `gesta changeset create` then `gesta changeset commit`, or `gesta stash save` to set it aside
```

`next` is one of `ready`, `commit-work`, `commit-pending` or `finish-rebase`, and the advice is
the command for that state. A stopped rebase outranks everything else, because nothing else is
safe until it is finished.

Answering this with separate commands costs four round trips and four blocks of output to keep
in context. One object is what an agent can branch on.

`--fix` acts on the two states that need no judgement: a snapshot that matches the disk is
committed, and one the disk has moved past is dropped rather than committed. Anything else is
refused with the reason and the command, because those choices lose data when guessed.

```sh
$ gesta brief --fix
committed cs68f126bd1a15

$ gesta brief --fix
left alone: 1 file(s) -- the working tree differs from the head and no snapshot describes it: snapshot it with `gesta changeset create`, or set it aside with `gesta stash save`
```

`--stash` adds one more action, and it takes a flag because it is the one that changes what a
person sees on disk: uncommitted work is set aside as a stash instead of refused. The work is
kept, the tree goes back to the head, and `gesta stash pop` brings it back.

```
$ gesta brief --fix --stash
stashed 2 file(s) as stash 1
$ gesta stash pop
applied stash 1 (2 file(s)): set aside by brief --fix
```

A stopped rebase is still refused even with the flag, since stashing mid rebase would hide a
conflict that has to be resolved first.

### Searching

```sh
gesta grep "read_config"                 # the working tree
gesta grep "read_config" --history       # every recorded version, deleted files included
gesta grep "hash" --word                # whole words, so `hashes` does not match
gesta grep "TODO" --path src/ --json     # a list of {path, line, rev, text}
```

`--history` is the one that has no equivalent in a working tree: the string that was deleted is
usually the one being looked for. Each distinct version is read once, so a body reached through
a hundred changesets costs one read.

An agent should use `--json` and read the locations, rather than parse a terminal's layout.

`--regex` reads the pattern as a regular expression:

```sh
gesta grep "^fn " --regex              # lines that start with a function signature
gesta grep "fn [a-z]+_config" --regex  # by shape
gesta grep "hash" --regex --word       # a regex that still respects word boundaries
```

The engine has no lookaround and no backreferences, because the Rust one does not. A pattern
that uses them is refused with the construct named and the position, rather than matching less
than you meant. Regex support is a build feature; a build without it says so instead of quietly
searching literally.

### Subrepositories

`sub add` records a path that points at another repository. The pointer holds the
remote and the changeset the child is pinned to; the working tree holds a real
checkout there.

The tree scan skips a subrepository before descending, so the child's files are
never recorded as the parent's own. That is the failure that makes nested
repositories dangerous, and it is covered by a test that asserts nothing under the
child is scanned while a sibling directory still is.

A missing child checkout is reported as `missing` and is not a deletion. Treating
it as one would let a single commit remove the pointer.

### Rebase

`rebase` replays each changeset of one branch onto another point. Each step is a
cherry-pick, so the merge logic is the one already tested; what is new is the
state between steps.

A rebase can stop part way when a pick conflicts. The state is stored, so
`--continue` and `--abort` work from a later command. Nothing is written to the
branch until a step succeeds, `--abort` returns the branch to where it started
rather than to the last successful step, and the original changesets are never
modified, so the old history stays readable.

### Cherry-pick

`cherry-pick` replays the change one changeset made onto another branch. The
three-way merge uses that changeset's own parent as the base, because the
question is what the changeset changed, not what two branches diverged on.

The pick records a new changeset rather than reusing the source id: the same id
on two branches would make every history query ambiguous. A conflicting pick
refuses, leaves the head where it was, writes the other side beside each file as
`.theirs`, and keeps your content untouched. Picking a change that is already
present records nothing.

### Bisect

`bisect` writes candidate changesets onto the working tree and asks whether each
one is good or bad. The answer can come in a later command, so a caller can test,
answer, and test again. The branch head never moves and the working tree is
restored when the search ends, whether it found the answer or was cancelled.

The order of candidates comes from the parent chain, never from creation time.
Commits made in one burst share a timestamp to the second, and a binary search
over an arbitrary order cuts the wrong half.

### Hooks

A hook is a program in `.gesta/hooks/`. Three are supported:

- `pre-commit` runs before anything is written. A non-zero exit stops the
  commit and leaves the repository untouched, so a refusal cannot half-apply.
- `post-commit` runs after the commit is durable. A non-zero exit is reported
  but does not undo the commit, because rolling back a finished write would make
  the store and the report disagree.
- `pre-land` runs before a branch merges into a shared target, which is when a
  change stops being private and the last point where a failing suite costs one
  person instead of everybody. A refusal leaves both branches untouched.

`pre-commit` and `post-commit` receive the operation and the changeset id as
arguments; `pre-land` receives the source and target branch names. All run with
the repository root as the working directory. Nothing runs unless a file with
the hook name exists, so a fresh repository executes nothing it did not ask for.

Hooks are ignored by the repository, so a clone never carries one and an
incoming bundle cannot install a program that runs on its own. Installing a hook
records its hash; a hook that changed after installation is refused rather than
executed, and `hook status` shows both hashes. Reinstalling is how a deliberate
edit is accepted.


Every read command takes `--json` (status, log, blame, history, diff, tag, ops, branch list, changeset list). Show commands print text by default because a terminal is still the common case.

## Storage

One file: `.gesta/gesta.db` (SQLite, WAL mode). No loose objects, no packfiles, no index to rebuild.

| Table | Purpose |
|---|---|
| `objects` | Content addressed blobs, zlib compressed when that shrinks the row |
| `changesets` | id, message, author, created_at, status, parent, parent2 (merges), branch, session, intent |
| `changeset_files` | Old and new hash per file inside a changeset |
| `file_versions` | Version chain per path (v1, v2, ...) |
| `blame` | Line ownership, recomputed on commit |
| `branches` | name, head changeset, origin, created_at |
| `tags` | name, changeset |
| `operations` | Pointer snapshots for `undo` |
| `ignore_rules` | Patterns added with `ignore add` |
| `remotes`, `remote_refs` | Remote urls and the last head seen per remote branch |
| `repo_state` | Key-value state such as current_branch |
| `worktree_cache` | `(path, mtime, size, hash)` per file, so a scan re-reads only what changed |

Dedup comes free from the hash: identical content is one row, however many files share it.

Compression is not applied blindly. A zlib stream carries a fixed header, so a short file grows when packed. `put_object` compares the packed size to the raw size and keeps whichever is smaller.

## Design notes

Why the shape is what it is, and what went wrong on the way: [docs/decisions.md](docs/decisions.md).

Changesets are explicit. `changeset create` snapshots the tree and `changeset commit` moves the head. An agent can look at what it is about to commit before it lands.

Blame is derived, not written per edit. On commit, `recompute_blame` walks the version chain oldest to newest and matches lines by sequence (longest common subsequence), not by content set. A line that duplicates another line keeps its own author.

Branches are pointers plus reachability. `status` and `latest_hash` walk the parent chain, so a file's current content is the newest version reachable from the head, not simply the newest version in the table.

Merge is real three way. It computes the merge base, applies both sides, writes the merged result to disk, records a changeset with two parents, and refuses to move the head if any file conflicts.

Text output is for a person reading a terminal. Anything that parses results should use `--json` or the MCP server, not scrape lines: a message containing a space or a quote would break a scraper, and a JSON parser does not care.

## Working with git repositories

A git repository can be read into this one and written back out, without calling the git
binary. A round trip keeps the commit ids: they are the hash of the tree, the parents, the
author line and the message, so equal ids prove all four survived.

Talking to a remote is the one thing that needs the git binary, and it is deliberate: the
objects are exported first and then `git push` sends them, so a token, an ssh key, a credential
helper and a forge's hooks all behave the way they already do.

```sh
gesta git import ../existing-project        # every branch, every commit
gesta git import ../existing-project --path-prefix src/
gesta git export ../git-export             # a repository git reads
gesta git export ../git-export --branch main
gesta git push git@github.com:me/project   # send to any git remote
gesta git pull https://github.com/me/project  # bring back what is new
```

```sh
$ gesta git import ../existing-project
imported 3 commit(s) as changesets, 5 path change(s), 0 already present
branches: feature, main

$ gesta git export ../git-export
exported 5 commit(s)
head: 028ccfd82fa6295f68053d2da35ac5ca8be11daa

$ cd ../git-export && git log --oneline --all
222e77b feature commit
028ccfd second commit
10e6dc0 first commit
```

Those ids are the same as the repository that was imported, and `git fsck` accepts the result.

Importing twice adds nothing: each changeset records the git commit it came from.

What is missing: packfiles. A repository whose objects are all in a pack has no loose objects to
read, so `git gc` (or `git repack -a -d`) gives them back first. Writing a pack reader is a
different piece of work and is not pretended to be here.

## Remote over HTTP

A repository can serve itself and another machine can clone, push and pull over plain HTTP:

```bash
# machine holding the repository
gesta serve --bind 0.0.0.0 --port 8787

# another machine
gesta remote clone http://server:8787 path/to/project
gesta remote add origin http://server:8787
gesta remote push origin
gesta remote pull origin
```

Endpoints: `GET /v1/health`, `GET /v1/branches`, `GET /v1/bundle?branch=X&have=...`, `POST /v1/bundle?branch=X`, `GET /v1/file?path=&rev=`, `POST /v1/changeset`.

A push that would move the server's branch to a head not containing its current head gets a 409 and the branch stays where it was.

## TLS

`https://` works. Two ways to get a certificate:

```bash
# a private network: generate one and pin its fingerprint
gesta serve --bind 0.0.0.0 --port 8787 --tls self-signed
# prints: pin this on clients with --pin 9f2c...

# a certificate you already have
gesta serve --bind 0.0.0.0 --port 8787 --tls /etc/zf/cert.pem --key /etc/zf/key.pem
```

A client verifies against the public roots by default, so a real certificate needs no flag. For a self-signed one, pin the fingerprint once:

```bash
gesta remote clone https://server:8787 project --token <token> --pin <fingerprint>
gesta remote add origin https://server:8787 --token <token> --pin <fingerprint>
```

The pin is stored with the remote, so a later push needs no flag. It is trust on first use made explicit, not "ignore certificate errors": the handshake still has to prove possession of the key that matches the pinned certificate, and a different certificate fails with the two fingerprints printed.

Three refusals are deliberate. A client with no pin rejects a self-signed certificate. A client with the wrong pin rejects it and says which fingerprints differ. `serve --tls` without tokens refuses to start, because an encrypted port with no access control is still an open port.

rustls with the ring backend is used rather than OpenSSL, so the build gains no second C toolchain beside the bundled SQLite.

## Access control

The server has two roles. A read token may clone and pull. A write token may also push. `/v1/health` is open so a monitor can check liveness without a credential.

```bash
# on the serving machine
gesta remote token add write alice --hashed
# prints the token once; only its digest is stored

# on a client
gesta remote clone http://server:8787 project --token <token>
gesta remote add origin http://server:8787 --token <token>
```

Tokens live in `.gesta/auth`, one entry per line, in either form:

```text
<token>:<role>:<name>              token in clear, hashed when loaded
sha256:<digest>:<role>:<name>      only the digest on disk
```

`--hashed` writes the second form. A leaked file then cannot be replayed, because presenting the digest does not authenticate; only its preimage does. A test pins that, and another pins that a malformed digest line never matches anything.

`.gesta/` is never tracked, so tokens cannot reach a changeset.

With no tokens configured the server refuses to bind to a non-loopback address. `0.0.0.0` without tokens exits with an error instead of serving an open port. A failed authentication waits 200 ms before answering, which blunts online guessing.

A token may carry a third field with a comma separated list of path globs. A write outside that list is refused with 403 and the path named in the message:

```text
<token>:write:docbot:docs/**,README.md
```

An empty scope means every path. A pattern without a slash matches the file name at any depth, so `*.md` covers `docs/a.md` as well as `a.md`.

## MCP server

`gesta mcp` speaks JSON-RPC 2.0 on stdin and stdout, one message per line, which is the stdio transport of the Model Context Protocol. A client discovers seven tools:

| Tool | Returns |
|---|---|
| `gesta_status` | branch, head, pending changesets, tracked path count |
| `gesta_log` | changesets newest first, with limit and branch |
| `gesta_read_file` | file content at a changeset or branch, without a checkout |
| `gesta_blame` | one entry per line: line number, changeset, author |
| `gesta_diff` | structured hunks for a changeset, or the working tree |
| `gesta_merge_conflicts` | what merging a branch would conflict on, without touching anything |
| `gesta_brief` | branch, head, next state, pending, stale snapshots, stashes |
| `gesta_fix` | commits a matching snapshot, drops a stale one, or sets work aside |
| `gesta_grep` | text in the working tree, or in every recorded version |
| `gesta_create_changeset` | a pending changeset from the working tree |
| `gesta_commit` | commits a pending changeset, moves the head |

Every result carries `structuredContent` with the JSON and the same text in `content`, so a client that only reads `content` still gets the data. A failing tool returns a JSON-RPC error with the reason, never a crash: unknown method is `-32601`, malformed JSON is `-32700`.

## Changesets without a working copy

An agent in a sandbox can write into the repository without cloning it:

```bash
curl -X POST http://server:8787/v1/changeset \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"branch":"main","message":"fix parser","author":"agent-7",
       "files":[{"path":"src/parse.rs","content":"..."}],
       "delete":["src/old.rs"],"commit":true}'
```

The blobs go straight into the object store and a changeset is recorded against the branch head. `commit: true` commits it in the same call. Nothing is written to the server's disk, so the response is not delayed by a checkout.

Two rules are enforced: a path that escapes the repository (`../`) is refused, and a path appearing twice in one request is refused because two contents for one file have no single meaning. A delete of a path that is already gone is reported in `skipped`, not silently dropped.

The comparison base is the tree the branch will have once every pending changeset commits, not just the head. Without that, a second call could not see what the first one wrote.

## Sparse checkout

A monorepo can be tens of gigabytes, and an agent that needs one module should not pay for the rest.

```bash
gesta sparse set crates/api docs/**
gesta sparse list
gesta sparse clear
```

A directory prefix selects its subtree, `**` crosses separators, and a pattern with no slash matches a file name at any depth.

The failure mode that matters is not "fewer files on disk". A file that is in the branch tree but was never written looks exactly like a file somebody deleted, so a naive scan would report the rest of the repository as deleted and the next commit would wipe it. The scope is applied to both sides of the comparison: the scan ignores disk paths outside it, and the deleted computation ignores committed paths outside it. Two checks in `bench/sparse.py` exist only for that, including a scope that matches nothing.

## Structured conflicts

Markers are readable by a person and awkward for an agent. `branch conflicts` reports the same conflict as data, without touching the working tree:

```bash
gesta branch conflicts feature --target main
```

Each file carries `ours`, `theirs` and `base` in full, plus one entry per conflicting region with `ours_start`, `ours_end`, `theirs_start`, `theirs_end`, the lines from each side, and three lines of context either way. An agent can choose per region and write the answer directly.

The same shape comes from `branch merge --json` and from the MCP tool `gesta_merge_conflicts`.

## Delta compression

A 500 KB file edited fifty times stores fifty versions. Storing each one whole costs fifty compressions of a file that changed in one line, so an object may be stored as a delta against the previous version of the same path: copy runs plus inserted lines, the same idea as a git packfile.

Two rules keep it safe. A delta is kept only when it is smaller than the compressed whole object, measured rather than assumed. And the delta is verified by rebuilding the target and comparing, so a bad encode returns nothing instead of storing a wrong version. On read, the rebuilt object is checked against its SHA-256 and a mismatch is an error, never silent corruption.

Measured on a 558 KB file edited one line fifty times: 1,504 KB before, 692 KB after, against git's 886 KB for the same history. That is 0.78x git, the first shape where a tree of this kind costs less than a packfile.

## Release gate

`bench/gate.py` is the single command that decides whether a version may be published. Each step is a real command with a real exit code:

```bash
python3 bench/gate.py            # all steps
python3 bench/gate.py audit      # one step
```

Three outcomes, three exit codes: every check passed, a check failed, or a check did not run. A
step name that matches nothing is refused with a suggestion rather than selecting an empty set, and
a check whose tool is missing is reported as a check that did not run instead of as a pass. A gate
that cannot tell "verified" from "not examined" reports on work nobody did.

| Step | Fails when |
|---|---|
| build | the release build does not finish |
| test | any unit test fails |
| clippy | any warning appears |
| audit | `cargo audit` reports an advisory |
| osv | `osv-scanner` reports an issue |
| coverage | line coverage drops below 55% |
| panics | `unwrap`, `expect`, `panic!`, `unreachable!` or `todo!` appears in production code |
| secrets | a key-shaped string or a credential assignment appears in `src` |
| paths | the guard against `../` in a remote path is gone |
| sql | SQL is assembled with `format!` outside the one placeholder helper |
| suites | any of the nine end to end suites fails |
| bughunter | an adversarial scenario fails |
| harness | a task passes without the work being done, or the reference answer fails |

Two of these exist because the first run of the gate found a real defect, which is the point of having it.

Dependency advisories found on the first run and closed: RUSTSEC-2026-0459 in the bundled SQLite (rusqlite 0.32 to 0.43) and RUSTSEC-2025-0134, an unmaintained PEM parser. The PEM parser is now in `tls.rs`, with eight tests on the base64 decoder because that is the kind of code that fails quietly.

Line coverage, measured with `cargo-llvm-cov --release --summary-only`: 60.64%. `cli.rs` and `cli_remote.rs` show 0% because unit tests never call them; the bench suites run the real binary instead, which is why the gate includes them.

## Verification

```bash
cargo test --release              # 417 unit tests
python3 bench/replace_git.py      # 20 end to end capability checks
python3 bench/http_remote.py      # 11 checks over a real socket
python3 bench/http_auth.py        # 18 checks on tokens and roles
python3 bench/mcp_json.py         # 23 checks on the MCP server and --json
python3 bench/api_changeset.py    # 13 checks on remote changesets and path scope
python3 bench/concurrency.py      # 8 checks on parallel snapshots and commits
python3 bench/scale.py            # an ordinary commit, against git
python3 bench/rebuild_cost.py     # the rebuild a merge and a land pay
python3 bench/conflicts.py        # 13 checks on structured conflict reports
python3 bench/sparse.py           # 16 checks on sparse checkout safety
python3 bench/tls.py              # 12 checks on TLS, including refusals
python3 bench/delta_vs_git.py     # disk usage against git on a long edit history
python3 bench/gate.py             # the release gate: 13 steps, exits non-zero on any failure
python3 bench/size_compare.py     # disk usage against git
python3 bench/bench.py            # speed baseline
```

What the suites cover, and why each check exists:

| Suite | Covers |
|---|---|
| unit | hashing, compressed object store, id allocation under concurrency, graph reachability across both parents of a merge, merge base, blame by sequence, glob matching, bundle serialisation, HTTP parsing, undo |
| `replace_git.py` | commit, diff, restore, show, deletion tracking, gitignore, disjoint merge, conflicted merge, branch switch, undo, tag, clone, push, fsck, compression, duplicate line blame, empty commit, binary reporting, large history, 8 concurrent snapshots |
| `http_remote.py` | server health, branch listing, clone, ignored files, push, landing on disk, refused rewind, pull with merge, second clone, 404 on unknown branch, https refusal |
| `http_auth.py` | token generation, hashed storage, anonymous refusal, read token cannot push, write token pushes, public bind without tokens refused |

## Performance

Measured in the workspace container, release build.

| Operation | 2,000 files | 5,000 files, 200 changesets |
|---|---|---|
| `changeset create` + `commit` | 54 ms | 117 ms |
| `status` | 45 ms | 54 ms |
| `blame` (201 versions of one file) | 12 ms | 26 ms |
| `log --limit 5` | 14 ms | 14 ms |
| `remote clone` over HTTP | 0.3 s | 0.3 s (5,000 files) |
| `status` on 8,000 files, one file edited | - | 483 ms, 7,999 from the stat cache |

An earlier version of `status` and `changeset create` asked for the committed hash of each file separately, and each lookup re-walked the whole parent chain. That made both operations quadratic in history depth: on 3,000 files, `status` took 55 ms at one changeset, 878 ms at fifty, and 6.8 s at two hundred. They now fetch the committed tree in one ranked query, so the cost no longer depends on how deep the history is.

Disk usage, 21 versions of a 42 KB file edited one line at a time: 256 KB against git's 79 KB, so 3.26x. Before compression it was 8.9x. A tree of 1,000 tiny files costs 6.08x git, because per-row overhead dominates when each object is a few dozen bytes. Both numbers are from `bench/size_compare.py`, reading the binary from the container's own disk.

`blame` does not slow down as history grows. It reads the precomputed table instead of replaying versions.

## Usage harness

`bench/harness` measures whether a person or an agent can do a day of version-control work with
gesta, rather than whether a command returns a number.

```bash
python3 bench/harness/run.py             # every task
python3 bench/harness/run.py --task tags # one task, and nothing if the name is wrong
python3 bench/harness/run.py --selftest  # does any check pass without the work?
```

Every task carries a goal, a setup that builds a starting repository, a reference answer, and
checks. The reference answer is what makes the checks meaningful: a check that has never been
passed cannot be told apart from a broken one. `--selftest` is the other half, running each setup
with no work done and requiring that not every check passes, because a check that always passes
measures agreement with itself.

A task ends as passed, failed, timed out, or broken, and the last two are recorded as their own
outcomes. A run stopped by a clock says nothing about the work, and scoring it as a wrong answer
blames the tool for the budget.

Commands run in a scrubbed environment: an allowlist plus an explicit list, with a secret-shaped
name dropped even when listed. Credentials are not on either list.

Writing this found three checks that performed the goal instead of verifying it, and one stash task
that could not tell its result from its starting state. See `docs/harness.md`.

## Limitations

- **TLS needs a certificate you supply or pin.** `serve --tls self-signed` works on a private network once a client pins the fingerprint; a public deployment needs a real certificate from a CA.
- **Tokens expire but are not revocable per session.** An entry can carry an expiry date, and removing its line revokes it, but there is no "revoke this one now" call.
- **No SSH transport.**
- **Small files cost more than git.** 14.91x for 1,000 files of seven bytes, pinned by the `storage-cost` gate step at 18x. On a real tree the ratio is far better: a markdown vault of 10,669 files costs 1.36x, down from 10.16x, because blame held one row per line, repeating a changeset and an author that had one home, and ranges replaced it. Per-row overhead dominates when every object is a few dozen bytes, so the second figure does not cancel the first. Packing several objects into one row was built and measured and made the file larger, because a pointer into a segment is a hash, two integers and a row header. All of it is in `docs/storage.md`.
- **Blame is line-sequence based.** A line moved and edited in the same commit loses its original author. A line that only moved keeps its author, and a file renamed with no edit keeps its whole history, including across a merge.
- **Binary files are stored but not diffed.** They are kept raw and restored byte for byte, and `blame` has nothing to attribute for them. A binary at or above 4 MB, or any path a `.gitattributes` rule marks `large`, lives beside the database in `.gesta/objects/aa/rest` rather than inside a table, so a backup or a `VACUUM` stops paying for it. A tree that holds one exports to git as a three-line LFS pointer, and `gesta serve` answers the batch API and the object route a client uses to resolve it. What is still absent is the lock API and any transfer adapter other than `basic`. See `docs/lfs.md`.
- **No submodules in the git sense.** `sub` records a child repository as a pointer, which covers the case without git's file.
- **SHA-1 git repositories only.** A repository made with `git init --object-format=sha256` is refused by name and told how to convert, rather than read as if the missing objects were corruption. Supporting the format means 64-character oids through the store and a different hash length in every pack trailer.
- **`gesta backup` / `gesta restore-repo` copy the whole store, not one branch.** Every branch, tag and blame range, verified before it replaces anything. See `docs/backup.md`.
- **`gesta serve` answers the git protocol well enough to clone and push.** `git clone` and `git push` against `http://host/git/repo` with a plain git client, over v0 or v2, with `multi_ack_detailed` negotiation and delta compression in the packs.
- **`gc` groups objects into shared compressed streams.** 50 to a group, sorted by size, because that gets 1.36x of the 1.50x available. Each object keeps its own row and hash.
- **Secret scanning refuses a commit rather than warning.** Rules, entropy floors and an allowlist; no network validation, because a tool that makes a key authenticate has sent the key somewhere.
- **`.gitattributes` covers line endings only.** `text`, `binary`, `eol=lf` and `eol=crlf`; no filters, diff drivers or merge drivers.
- **Conflicts are resolved by editing files**, then committing the result. There is no interactive resolver.
- **Large histories are still lightly measured.** Behaviour up to 5,000 files and 200 changesets is known. Beyond that, unmeasured.

## Layout

```
src/main.rs        CLI definitions and the dispatcher
src/cli.rs         command handlers
src/cli_remote.rs  remote, fsck, stats, ignore, serve handlers
src/repo.rs        changeset lifecycle, blame, merge, checkout
src/store.rs       schema, compressed object store, graph queries
src/merge.rs       line diff and three way merge
src/remote.rs      bundle format and application
src/http.rs        HTTP server and client
src/ops.rs         operation log and undo
src/worktree.rs    ignore matching and file scanning
src/cache.rs       working tree scan with a (mtime, size, hash) cache
src/content.rs     changesets built from supplied content, no working tree
src/json.rs        JSON writer and parser, no serde
src/mcp.rs         MCP server over stdio
src/conflict.rs    structured conflict reports
src/sparse.rs      working set patterns and sparse checkout
src/delta.rs       line delta encode, apply, serialise
src/tls.rs         TLS server and client, certificate pinning
```

Build environment: Docker container `hermes-workspace`, Rust 1.100.0, image `hermes-workspace-rust:latest`.

The source lives on the host at `D:/Hermes/gesta/` and is mounted into the container at `/work/gesta`, so recreating the container does not lose it.

## Correctness guarantees

Each of these is pinned by a test, and each one exists because the first implementation got it wrong.

- **Ids are unique.** Allocation mixes a SQLite-side counter (`RETURNING` on an upsert) with the message, author and a sequence, so concurrent agents writing the same message in the same second still get distinct ids.
- **Commits are atomic.** File versions, blame, the changeset status and the branch head move inside one transaction. A missing object aborts the whole commit.
- **History does not inflate.** A snapshot matching the recorded content adds no version, and version numbers stay contiguous.
- **No changeset is orphaned.** A pending changeset is re-parented onto the live head at commit time, so two snapshots taken while the branch was empty still chain instead of forking.
- **Diffs keep order and repeats.** `line_diff` works on an alignment, so duplicates are reported where they occur.
- **Binary files are reported, not dropped.** They show up in `status` and in `changeset create` output.
- **SQL errors propagate.** No `unwrap_or` masks a failed query.
- **Unattributed lines are marked.** `blame` prints `?` and a warning instead of inventing an owner.
- **A push cannot rewind a branch.** The server applies the bundle, checks that the incoming head contains the previous one, and rolls the ref back if it does not.
- **A pull cannot delete local work.** Fetching applies objects without touching the branch, then merges a tracking branch into the local one.
- **Concurrent writers succeed.** `busy_timeout` covers a reader waiting for a writer but not two writers arriving at once, so mutating commands retry a busy database with a growing pause. Before that, 20 parallel commits produced 19 `database is locked` failures; now all 20 land.
- **A deleted file stays deleted.** Content lookups read `changeset_files`, where a deletion is a row with no hash, not `file_versions`, which has no row for a deletion at all. Reading versions alone resurrected deleted files.
- **A queued changeset is visible to the next one.** The base for a new changeset is the effective tree, so a second API call can edit or delete what the first one wrote before either is committed.
- **A sparse scope never deletes anything.** A path outside the working set is not on disk by design, so its absence is not a deletion; reporting it as one would let one commit remove the rest of the repository.
- **A delta is verified before it is used.** Encoding rebuilds the target, and reading re-checks the hash, so a corrupt delta is an error rather than wrong content.
- **A custom certificate verifier still proves key possession.** Pinning a fingerprint skips the chain check, not the handshake signature.
- **A revision can be a branch, a tag or a changeset.** One resolver answers all three, so `show main:x.txt` reads the branch head instead of reporting a file that is plainly there.

## Status

Usable for one developer or a set of agents, on one machine, on a private network over TLS, or by agents that never check out the tree. A markdown vault of 10,669 files costs 1.36x git in disk, down from 10.16x after blame storage moved from one row per line to ranges; a tree of tiny files costs 14.91x, which is the honest other half of that sentence. A plain `git clone` and a plain `git push` work against `gesta serve`, over v0 and v2 with delta packs. Still not a full git replacement: the LFS lock API and submodules do not exist, and a removed token stops working when the server next reloads the auth file. The version number lives in `Cargo.toml`; one written here goes stale on the next release.

`CHANGELOG.md` says what changed at each version. `docs/upgrade-plan.md` holds the measurements behind each decision and the list of what is still missing.

## License

MIT. See `LICENSE`.

Bug reports and changes: `CONTRIBUTING.md`. Security reports: `SECURITY.md`.
