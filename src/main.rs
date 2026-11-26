//! gesta: source control for AI agents.
//!
//! Server-side changesets, content-addressed compressed objects, derived blame
//! and history, three-way merge, remote sync, and an operation log for undo.

mod cli;
mod repo;
mod store;
mod support;
mod transport;

use clap::{Parser, Subcommand};
use cli::cli_remote;
use std::error::Error;
use std::path::PathBuf;
use support::clock;
use support::error;

use store::{init_schema, migrate, open_db};

/// Directory that holds the repository. Named after the tool, and the old name is
/// still recognised so repositories created before the rename keep working.
pub const ZF_DIR: &str = ".gesta";
/// The directory this project used before it was renamed. Reading it is
/// supported; nothing ever writes to it.
pub const LEGACY_DIR: &str = ".zenforge";

/// The repository directory in `root`, whatever name it was created under.
///
/// Every path built against `ZF_DIR` would look in `.gesta/` and miss a
/// repository made before the rename, so callers ask here instead. When neither
/// exists the new name is returned, which is what `init` then creates.
/// The database file for the repository in `root`, under whichever name the
/// repository was created with.
///
/// The directory name and the file name changed together, so a repository made
/// before the rename has `.zenforge/zenforge.db`. Looking only for `.gesta` and
/// `gesta.db` finds nothing inside an existing repository, and the schema init
/// then creates an empty database next to the real one, which looks like every
/// file went missing.
pub fn repo_db(root: &std::path::Path) -> PathBuf {
    let new = store_dir(root).join(ZF_DB);
    let legacy = root.join(LEGACY_DIR).join(LEGACY_DB);

    // Size, not existence. An empty file of the new name is what a failed or
    // interrupted start leaves behind, and preferring it over the real database
    // next to it makes every repository look empty.
    let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if size(&new) > 0 {
        return new;
    }
    if size(&legacy) > 0 {
        return legacy;
    }
    new
}

pub fn repo_dir(root: &std::path::Path) -> PathBuf {
    let new = root.join(ZF_DIR);
    if new.is_dir() {
        return new;
    }
    let old = root.join(LEGACY_DIR);
    if old.is_dir() {
        return old;
    }
    new
}
pub const ZF_DB: &str = "gesta.db";
/// Database file name used before the rename, inside `LEGACY_DIR`.
pub const LEGACY_DB: &str = "zenforge.db";

pub type ZResult<T> = std::result::Result<T, Box<dyn Error>>;

pub fn bad(msg: impl Into<String>) -> Box<dyn Error> {
    msg.into().into()
}

pub fn now() -> i64 {
    clock::unix_now()
}

fn fmt_ts(ts: i64) -> String {
    clock::format_minute(ts)
}

pub fn normalise(file: &str) -> String {
    // The separator is a platform fact. On Windows a user types `src\\main.rs` and means the
    // directory separator. On Unix a backslash is an ordinary character in a file name, so
    // rewriting it turns `back\\slash.txt` into a different file that may not exist, and a read
    // that worked before the path fix stopped working after it.
    if cfg!(windows) {
        file.replace('\\', "/")
    } else {
        file.to_string()
    }
}

pub fn repo_root() -> ZResult<PathBuf> {
    let cur = std::env::current_dir()?;
    let mut probe = cur.clone();
    loop {
        // A repository made before the rename has the old directory name. Reading
        // it keeps every existing repository usable; `gesta init` and every write
        // path use the new name, so nothing new lands in the old directory.
        if probe.join(ZF_DIR).is_dir() || probe.join(LEGACY_DIR).is_dir() {
            // The root of the working tree, not the directory the process started in: run from a
            // subdirectory and the files still belong at the top. Where the STORE is, is a separate
            // question that `store_dir` answers.
            return Ok(probe);
        }
        // A linked worktree marks itself with a `.gesta` FILE naming the main store, the way git uses
        // a `.git` file. That is what makes a second tree cost a pointer instead of a copy of every
        // object.
        if probe.join(ZF_DIR).is_file() {
            let text = std::fs::read_to_string(probe.join(ZF_DIR)).map_err(|e| {
                Box::new(error::Error::new(
                    error::Code::NotARepo,
                    format!("cannot read {}: {e}", probe.join(ZF_DIR).display()),
                ))
            })?;
            if text.trim().is_empty() {
                return Err(Box::new(error::Error::new(
                    error::Code::NotARepo,
                    format!(
                        "{} is empty, so this worktree does not name a repository",
                        probe.join(ZF_DIR).display()
                    ),
                )));
            }
            return Ok(probe);
        }
        if !probe.pop() {
            return Err(Box::new(error::Error::new(
                error::Code::NotARepo,
                format!(
                    "not a repository: {} and no directory above it holds one (run `gesta init` here)",
                    cur.display()
                ),
            )));
        }
    }
}

/// The store a working tree belongs to.
///
/// For an ordinary repository this is `<root>/.gesta`. For a linked worktree it is the path written in
/// the `.gesta` file, which points at the main tree's store: one database, two working trees, so a
/// commit made in either is immediately visible in the other.
pub fn store_dir(root: &std::path::Path) -> PathBuf {
    let marker = root.join(ZF_DIR);
    if marker.is_file() {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            let target = text.trim();
            if !target.is_empty() {
                let p = std::path::PathBuf::from(target);
                return if p.is_absolute() { p } else { root.join(p) };
            }
        }
    }
    repo_dir(root)
}

/// Open the repository at the current directory, running any pending migration.
fn open_repo() -> ZResult<(PathBuf, rusqlite::Connection)> {
    let root = repo_root()?;
    let conn = open_db(&root)?;
    let migrated = migrate(&conn)?;
    if migrated {
        init_schema(&conn)?;
    }
    // A linked worktree holds its own branch. Read it once, here, so every command in this process
    // that asks "which branch am I on" gets this tree's answer rather than the main tree's.
    if root.join(crate::repo::worktree::ZF_BRANCH).is_file() {
        let branch = crate::repo::worktree::branch_of(&root, &conn)?;
        crate::store::set_worktree_branch(&branch);
    }
    Ok((root, conn))
}

// CLI.

#[derive(Parser)]
#[command(name = "gesta", version, about = "Source control for AI agents")]
struct Cli {
    /// Print machine-readable JSON instead of text
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum GitCmd {
    /// Read a git repository into this one
    Import {
        /// The git repository, or a directory inside it
        #[arg(default_value = ".")]
        path: String,
        /// Only paths under this prefix
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// Write this repository out as a git repository git can read
    Export {
        /// Where to write it; created if missing
        path: String,
        /// One branch, or every branch when omitted
        #[arg(long)]
        branch: Option<String>,
    },
    /// Push branches to a git remote through the git binary
    Push {
        /// Any git remote: a URL, an ssh path, or a forge
        remote: String,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Fetch branches from a git remote and import them
    Pull {
        /// Any git remote: a URL, an ssh path, or a forge
        remote: String,
        /// Only paths under this prefix
        #[arg(long)]
        path_prefix: Option<String>,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository in the current directory
    Init {
        /// Create a bare repository with no working tree
        #[arg(long)]
        bare: bool,
    },
    /// Replace the author on existing changesets, for a push that already disclosed one
    Anonymise {
        /// Author string to replace. Repeat for several. With none, every changeset on the branch.
        #[arg(long = "author")]
        authors: Vec<String>,
        /// The author string to put in its place
        #[arg(long, default_value = "anonymous")]
        to: String,
        #[arg(long, default_value = "main")]
        branch: String,
        /// Report what would change without changing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Show repository state and working-tree changes
    Status,
    /// Show changes not yet staged in a changeset (like git diff)
    Diff {
        /// Compare against this changeset instead of the branch head
        #[arg(long)]
        changeset: Option<String>,
        /// Compare from this revision (a branch, tag or changeset) to --to
        #[arg(long)]
        from: Option<String>,
        /// Compare to this revision. Defaults to the working tree
        #[arg(long)]
        to: Option<String>,
        /// Only report names
        #[arg(long)]
        name_only: bool,
    },
    /// List changesets, newest first
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// One line per changeset
        #[arg(long)]
        oneline: bool,
        /// With --json, wrap the list in an object that names its schema
        #[arg(long)]
        envelope: bool,
    },
    /// Changeset operations
    Changeset {
        #[command(subcommand)]
        sub: ChangesetCmd,
    },
    /// Show which changeset introduced each line of a file
    Blame {
        file: Option<String>,
        /// Revision to attribute to the change before it, instead of itself. Repeat for several,
        /// or name a file of revisions. With none, .gesta-blame-ignore-revs at the root is read.
        #[arg(long = "ignore-rev")]
        ignore_rev: Vec<String>,
    },
    /// Show version history of a file
    History { file: Option<String> },
    /// Show a file as of a changeset or branch
    Show {
        /// Path, optionally as `rev:path`
        spec: Option<String>,
        /// Print a JSON object instead of the file's bytes. A binary body is base64, and `binary`
        /// says so, because a terminal writes raw bytes without a word about what they are.
        #[arg(long)]
        json: bool,
    },
    /// List the files a revision holds
    Ls {
        /// A branch, a tag or a changeset; the working branch when omitted
        rev: Option<String>,
        /// Only paths under this prefix
        #[arg(long)]
        path: Option<String>,
        /// Print the content hash beside each path
        #[arg(long)]
        long: bool,
    },
    /// Import from or export to a git repository, without calling git
    Git {
        #[command(subcommand)]
        sub: GitCmd,
    },
    /// Everything a session needs before its first edit, in one call
    Brief {
        /// Print the object instead of a short summary
        #[arg(long)]
        json: bool,
        /// Do the safe part: commit snapshots that match the disk, drop ones it moved past
        #[arg(long)]
        fix: bool,
        /// With --fix, set uncommitted work aside as a stash instead of refusing it
        #[arg(long, requires = "fix")]
        stash: bool,
    },
    /// Search the working tree, or every version in the history
    Grep {
        /// Text to find
        pattern: Option<String>,
        /// Search every recorded version instead of the files on disk
        #[arg(long)]
        history: bool,
        #[arg(long)]
        ignore_case: bool,
        /// Match whole words only
        #[arg(long)]
        word: bool,
        /// Read the pattern as a regular expression
        #[arg(long)]
        regex: bool,
        /// Only paths starting with this prefix
        #[arg(long)]
        path: Option<String>,
        /// How many versions to read when searching history
        #[arg(long, default_value_t = 5000)]
        limit: i64,
        /// Print `path:line` only, for piping into another tool
        #[arg(long)]
        files_with_matches: bool,
        /// Cut each matching line to this many characters. Zero keeps lines whole.
        #[arg(long = "max-line", default_value_t = 0)]
        max_line: usize,
    },
    /// Write files back to disk from a changeset
    Checkout {
        /// A file to write back, or `.` for everything on the branch. Defaults to `.`, because
        /// writing the branch out is the reason the command exists and it cannot lose work that
        /// was never committed - the branch already holds it
        file: Option<String>,
        #[arg(long)]
        changeset: Option<String>,
    },
    /// Discard uncommitted changes (like git restore)
    Restore {
        /// File to put back, or `.` for the whole working tree. Required on purpose: with no
        /// argument this command would discard everything uncommitted, and a safety net that
        /// fires on a typo is not a safety net
        file: Option<String>,
    },
    /// Write the whole repository to one file: every branch, tag and blame range
    Backup {
        /// Destination file. Replaced if it exists
        dest: Option<String>,
        #[arg(long)]
        json_backup: bool,
    },
    /// Encrypted backups to an S3 bucket, and back
    BackupRemote {
        #[command(subcommand)]
        sub: BackupRemoteCmd,
    },
    /// Check a backup file without touching the repository
    VerifyBackup {
        /// The backup file
        file: Option<String>,
    },
    /// Replace this repository with the one in a backup file
    RestoreRepo {
        /// The backup file
        file: Option<String>,
        /// Required. A restore replaces the store, so it refuses without this and asks nothing:
        /// read the counts it prints back, then run it again with --yes
        #[arg(long)]
        yes: bool,
    },
    /// Replay this branch's changesets onto another point
    Rebase {
        /// Branch or changeset to replay onto. Not needed with --continue or
        /// --abort, which work from the saved state.
        onto: Option<String>,
        /// Branch to rebase. Defaults to the current branch.
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
        /// Finish a stopped rebase using what is on disk
        #[arg(long)]
        continue_: bool,
        /// Put the branch back where the rebase started
        #[arg(long = "abort")]
        abort: bool,
    },
    /// Apply the change one changeset made onto the current branch
    CherryPick {
        /// Changeset id, tag, or branch name
        source: Option<String>,
        /// Branch to apply onto. Defaults to the current branch.
        #[arg(long)]
        onto: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Search the history for the changeset that introduced a fault
    Bisect {
        #[command(subcommand)]
        sub: BisectCmd,
    },
    /// Manage paths that point at other repositories
    Sub {
        #[command(subcommand)]
        sub: SubCmd,
    },
    /// Save the working tree aside and reset it to the head
    Stash {
        #[command(subcommand)]
        sub: StashCmd,
    },
    /// Manage hooks that run before and after a commit
    Hook {
        #[command(subcommand)]
        sub: HookCmd,
    },
    /// Undo the last operation (like git reset but reversible both ways)
    Undo,
    /// Put back the most recently undone operation
    Redo,
    /// Undo the effect of one changeset by recording a new one
    Revert {
        id: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Show the operation log
    Ops {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Tag a changeset
    Tag {
        /// Name of the tag. Omit it with --list.
        name: Option<String>,
        /// Revision to tag: a changeset, a branch or another tag. Defaults to the branch head.
        #[arg(long)]
        changeset: Option<String>,
        #[arg(long)]
        list: bool,
        /// Move the tag if it already exists, instead of refusing
        #[arg(long)]
        force: bool,
    },
    /// Branch operations
    Branch {
        #[command(subcommand)]
        sub: BranchCmd,
    },
    /// Annotations attached to a changeset, kept apart from it
    Notes {
        #[command(subcommand)]
        sub: NotesCmd,
    },
    /// Additional working trees that share this repository's store
    Worktree {
        #[command(subcommand)]
        sub: WorktreeCmd,
    },
    /// Remote operations (push, pull, clone)
    Remote {
        #[command(subcommand)]
        sub: RemoteCmd,
    },
    /// Push to a forge (GitHub, GitLab, Gitea) without the git binary
    Forge {
        #[command(subcommand)]
        sub: ForgeCmd,
    },
    /// Manage stored credentials for remotes
    Secret {
        #[command(subcommand)]
        sub: SecretCmd,
    },
    /// Land a branch onto a target, refusing if the target moved since it was read
    Land {
        /// Branch whose changes are being landed
        source: Option<String>,
        /// Branch being landed onto
        #[arg(long, default_value = "main")]
        onto: String,
        /// The head read before the merge, which the write must still find
        #[arg(long)]
        expect: Option<String>,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Which merge brought a changeset into a branch, and when
    ///
    /// Answers "was this landed, and where" from the recorded generations when it can and from a
    /// walk of the first-parent chain when it cannot. Not finding it is an answer, not an error:
    /// work that was never landed reads as such.
    Landed {
        /// The changeset to look for
        changeset: Option<String>,
        /// The branch it should be on
        #[arg(long, default_value = "main")]
        branch: String,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a stack of changesets step by step, with a risk read on each
    Review {
        /// Start after this revision; everything after it is shown
        #[arg(long)]
        from: Option<String>,
        /// End at this revision. Defaults to the branch head
        #[arg(long)]
        to: Option<String>,
        /// Show only steps at this band or above: low, medium, high
        #[arg(long)]
        min_risk: Option<String>,
    },
    /// Run an MCP server on stdio, exposing the repository as tools
    Mcp,
    /// Serve this repository over HTTP
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Serve over TLS. Give `self-signed` to generate a certificate, or the
        /// path to a PEM certificate.
        #[arg(long)]
        tls: Option<String>,
        /// Private key for `--tls`, required when the certificate is given as a
        /// path. Ignored for `self-signed`.
        #[arg(long)]
        key: Option<String>,
    },
    /// Present a branch as a read-only drive, without checking it out
    #[cfg(windows)]
    Mount {
        /// Drive letter or directory to mount on, for example X: or C:\\mnt\\gesta
        target: String,
        /// Branch to show; defaults to the current one
        #[arg(long)]
        branch: Option<String>,
    },
    /// Limit the working tree to a set of paths
    Sparse {
        #[command(subcommand)]
        sub: SparseCmd,
    },
    /// Remove unreachable objects and vacuum the database
    Gc,
    /// Manage the key that signs changesets
    Key {
        #[command(subcommand)]
        sub: KeyCmd,
    },
    /// Check a changeset's signature against a public key
    Verify {
        /// Changeset id, or the current branch head when omitted
        id: Option<String>,
        /// The public key to check against, hex. Defaults to the one recorded with it.
        #[arg(long)]
        key: Option<String>,
    },
    /// Verify repository integrity
    Fsck {
        /// Delete objects nothing references
        #[arg(long)]
        prune: bool,
        /// Also compare the working tree with the head. Off by default because it reads
        /// every tracked file.
        #[arg(long)]
        worktree: bool,
    },
    /// Report storage statistics
    Stats,
    /// Manage git submodules
    Submodule {
        #[command(subcommand)]
        sub: SubmoduleCmd,
    },
    /// Ignore patterns
    Ignore {
        #[command(subcommand)]
        sub: IgnoreCmd,
    },
}

#[derive(Subcommand)]
enum ChangesetCmd {
    /// Snapshot the working tree as a pending changeset
    Create {
        message: String,
        #[arg(long, default_value = "agent")]
        author: String,
        /// Limit the snapshot to these paths. Repeat to name more. Without it the snapshot is the
        /// whole tree, which is what one person wants and what two agents in one checkout cannot
        /// share: each would record the other's work as its own.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Which session produced this change, so the reasoning stays attached.
        /// Defaults to GESTA_SESSION when that is set.
        #[arg(long)]
        session: Option<String>,
        /// Why the change was made, in one line
        #[arg(long)]
        intent: Option<String>,
    },
    /// Commit a pending changeset onto the current branch
    /// Commit a pending changeset onto the current branch. With no id, commits the only pending
    /// one, and lists them when there are several.
    Commit { id: Option<String> },
    /// Commit all pending changesets up to and including this one
    Apply { id: String },
    /// Commit every pending changeset in creation order
    CommitAll,
    /// Show what a changeset changed
    Diff { id: String },
    /// List pending changesets
    List,
    /// Sign a pending changeset with the local key
    Sign {
        id: String,
        /// Where the signing key lives; defaults to ~/.gesta/signing.key
        #[arg(long)]
        key: Option<String>,
    },
    /// Sign off a changeset so it may be pushed to a protected branch
    ///
    /// The approval names who signed it, and a protected branch refuses a push whose only approval is
    /// from the credential doing the pushing.
    Approve {
        id: String,
        /// Who is signing off. A reviewer's name, not the author's.
        #[arg(long, default_value = "reviewer")]
        by: String,
    },
    /// Show who has signed off on a changeset
    Approvals { id: String },
    /// Find the changeset that holds a given git commit
    ///
    /// A commit imported from git has a changeset id of its own, and the two names are not derivable
    /// from one another. Naming the changeset for a commit therefore needs this lookup.
    Find {
        /// The git commit id, full or abbreviated
        #[arg(long)]
        commit: String,
    },
}

#[derive(Subcommand)]
enum BisectCmd {
    /// Begin a search between a known good and a known bad changeset
    Start {
        good: String,
        bad: String,
    },
    /// Report the changeset under test as good, or as bad
    Good,
    Bad,
    /// Drive the search by running a command at each candidate
    ///
    /// Exit 0 means good, any other exit means bad, and 125 skips a candidate that cannot be tested.
    Run {
        /// Command to run at each candidate; its exit code is the answer
        command: String,
    },
    /// Show the state of the search
    Status,
    /// Abandon the search and put the working tree back
    Cancel,
}

/// Moving an encrypted backup off the machine the repository lives on.
///
/// A backup on the same disk as the store protects against a bad commit and not against a dead disk,
/// which is the failure it most needs to survive.
#[derive(Subcommand)]
pub enum BackupRemoteCmd {
    /// Snapshot, seal and upload
    Push {
        /// S3 prefix, e.g. `backups`
        #[arg(long, default_value = "backups")]
        prefix: String,
        /// Keeps the newest N and deletes the rest
        #[arg(long, default_value_t = 7)]
        keep: usize,
        /// Passphrase the backup is sealed with. Read from GESTA_BACKUP_PASSPHRASE when omitted
        #[arg(long)]
        passphrase: Option<String>,
        #[arg(long)]
        json_remote: bool,
    },
    /// List what is in the bucket
    List {
        #[arg(long, default_value = "backups")]
        prefix: String,
        #[arg(long)]
        json_remote: bool,
    },
    /// Download, unseal and restore
    Pull {
        /// The key to restore, or `latest`
        key: String,
        #[arg(long, default_value = "backups")]
        prefix: String,
        #[arg(long)]
        passphrase: Option<String>,
        /// Skip the question. Without it, restoring asks first because it replaces the store
        #[arg(long)]
        yes: bool,
    },
    /// Delete everything past the newest N
    Prune {
        #[arg(long, default_value = "backups")]
        prefix: String,
        #[arg(long, default_value_t = 7)]
        keep: usize,
    },
}

#[derive(Subcommand)]
enum SubCmd {
    /// Record a path as a subrepository
    Add {
        path: String,
        /// Where the child repository lives
        remote: String,
    },
    /// Show the subrepositories and what each is pinned to
    List,
    /// Pin a subrepository to the child's current head
    Pin { path: String },
    /// Stop tracking a path as a subrepository
    Remove { path: String },
}

#[derive(Subcommand)]
enum SubmoduleCmd {
    /// Add a submodule from a remote URL
    Add {
        /// Path where the submodule will be checked out
        path: String,
        /// Remote URL to clone from
        url: String,
        /// Branch to track (optional)
        #[arg(long)]
        branch: Option<String>,
    },
    /// Initialize submodules: clone missing ones at their pinned commit
    Init,
    /// Update submodules to their pinned commit
    Update,
    /// Show status of submodules
    Status,
    /// Sync submodule URLs from .gitmodules to the local config
    Sync,
}
#[derive(Subcommand)]
enum StashCmd {
    /// Save the working tree aside and reset it to the head
    Save {
        #[arg(long, default_value = "wip")]
        message: String,
        #[arg(long, default_value = "agent")]
        author: String,
        /// Stash only these paths, leaving everything else in place
        ///
        /// Repeat the flag to name several. This is the whole-file form: a caller that knows which
        /// files to set aside says so. To keep part of one file, use `--hunk`.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Stash one hunk of a file, as `path:number` where the number comes from `gesta diff`
        ///
        /// Repeat the flag to name several, in one file or across files. This is the case `--path`
        /// cannot express: two unrelated edits in one file, one finished and one an experiment.
        #[arg(long = "hunk")]
        hunks: Vec<String>,
    },
    /// List saved stashes, newest first
    List,
    /// Write the newest stash back, or a chosen one by id
    Pop {
        #[arg(long)]
        id: Option<i64>,
    },
    /// Delete a stash without applying it
    Drop {
        #[arg(long)]
        id: Option<i64>,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// List the hooks and whether each one is installed
    List,
    /// Write a hook from a file or from standard input
    Install {
        name: String,
        /// Path to a script. Without it, the body is read from standard input.
        #[arg(long)]
        file: Option<String>,
    },
    /// Remove a hook
    Remove { name: String },
    /// Show each hook and whether it changed since it was installed
    Status {
        /// Answer as JSON
        #[arg(long)]
        json_status: bool,
    },
}

#[derive(Subcommand)]
enum SparseCmd {
    /// Set the working set and check those paths out
    Set { patterns: Vec<String> },
    /// Show the current working set
    List,
    /// Return to a full checkout
    Clear,
}

#[derive(Subcommand)]
enum NotesCmd {
    /// Attach a note to a changeset, replacing any note it already carries
    Add {
        /// The changeset the note belongs to
        changeset: String,
        /// The text of the note
        #[arg(short, long)]
        message: String,
    },
    /// Print the note a changeset carries
    Show { changeset: String },
    /// Drop the note a changeset carries, leaving the changeset alone
    Remove { changeset: String },
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// Add a working tree on another branch
    Add {
        /// Where to put it; created if missing
        path: Option<String>,
        /// The branch to check out there
        branch: Option<String>,
    },
    /// List the working trees of this repository
    List,
    /// Remove a working tree, leaving its store alone
    Remove {
        /// The working tree to remove
        path: Option<String>,
        /// Remove it even though it holds uncommitted work
        ///
        /// Without this, a tree with edits that are not in a changeset is refused: those edits exist
        /// only on disk, and removing the directory deletes the only copy with no way back.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum BranchCmd {
    Create {
        name: Option<String>,
        #[arg(long, default_value = "main")]
        from: String,
    },
    List,
    Switch {
        name: Option<String>,
    },
    Delete {
        name: Option<String>,
    },
    Merge {
        source: Option<String>,
        #[arg(long, default_value = "main")]
        target: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    Info {
        name: Option<String>,
    },
    /// Protect a branch so a push to it needs a sign-off from another credential
    Protect {
        name: String,
        /// Remove the protection instead of adding it
        #[arg(long)]
        unset: bool,
    },
    /// Report what a merge would conflict on, without touching anything
    Conflicts {
        source: String,
        #[arg(long, default_value = "main")]
        target: String,
    },
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Ask for a passphrase and create the encrypted store
    Init,
    /// Set one entry; the value is read from the terminal, never from an argument
    Set {
        /// Entry name, for example s3.e2.access_key
        name: String,
    },
    /// List entry names. Values are never printed.
    List,
    /// Remove one entry
    Remove { name: String },
    /// Look for anything that looks like a credential
    Scan {
        /// Read every stored version, so a secret that was deleted still shows up
        #[arg(long)]
        history: bool,
        /// Machine readable
        #[arg(long = "json")]
        json_scan: bool,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Create a signing key, refusing to overwrite one that exists
    Generate {
        /// Where to write it; defaults to ~/.gesta/signing.key
        #[arg(long)]
        path: Option<String>,
    },
    /// Replace the active signing key, archiving the old one instead of deleting it
    ///
    /// A signature names the public key it was made with, so nothing has to be re-signed. The old
    /// private key is kept because a disputed signature is worth being able to check by hand.
    Rotate {
        /// Where to write the new key; defaults to the one in use
        #[arg(long)]
        path: Option<String>,
        /// Report what would happen without touching a file
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the keys this repository has used and which one is active
    List,
}

#[derive(Subcommand)]
enum ForgeCmd {
    /// Push one branch to a forge over the git smart HTTP protocol
    Push {
        /// The repository URL, without a trailing slash: https://github.com/owner/repo
        url: String,
        /// The branch to push
        #[arg(default_value = "main")]
        branch: String,
        /// The ref name on the remote
        #[arg(long, default_value = "refs/heads/main")]
        remote_ref: String,
        /// Push every branch, each to a ref of the same name, in one request
        ///
        /// One command carrying several refs is what `git push --all` sends, and it is one pack. A
        /// branch the remote already has at the same commit is skipped rather than sent again.
        #[arg(long)]
        all: bool,
        /// A token with write access. Falls back to GESTA_FORGE_TOKEN.
        #[arg(long)]
        token: Option<String>,
        /// The username paired with the token. GitHub and Gitea ignore it; GitLab wants oauth2.
        #[arg(long, default_value = "x-access-token")]
        user: String,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RemoteCmd {
    Add {
        name: String,
        url: String,
        /// Token the server requires, stored with the remote.
        #[arg(long)]
        token: Option<String>,
        /// SHA-256 fingerprint of the server certificate, for a self-signed one.
        #[arg(long)]
        pin: Option<String>,
    },
    List,
    /// Manage access tokens for serving over HTTP
    Token {
        #[command(subcommand)]
        sub: TokenCmd,
    },
    /// List the branches a remote holds, for remotes that can be asked
    Ls {
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Extend the history of a shallow clone by taking older changesets
    Deepen {
        #[arg(long)]
        depth: Option<usize>,
        /// Take everything, removing the shallow boundary
        #[arg(long)]
        unshallow: bool,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
        /// Only report what would be fetched
        #[arg(long)]
        dry_run: bool,
    },
    /// Say what a push would send, without sending it
    Inspect {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
        /// Answer as JSON
        #[arg(long)]
        json_inspect: bool,
        /// Exit non-zero when the payload would carry author identities
        #[arg(long)]
        check_identity: bool,
    },
    /// Send this repository's branch to a remote
    Push {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Fetch a remote branch and merge it into the current branch
    Pull {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },
    /// Copy a remote repository into a new directory
    Clone {
        url: String,
        dir: String,
        #[arg(long)]
        token: Option<String>,
        /// SHA-256 fingerprint of the server certificate, for a self-signed one.
        #[arg(long)]
        pin: Option<String>,
        /// Take only the newest N changesets, for a history too long to download whole
        #[arg(long)]
        depth: Option<usize>,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Generate a token and write it to .gesta/auth
    Add {
        /// read or write
        #[arg(default_value = "write")]
        role: String,
        /// Label shown in `token list`
        name: String,
        /// Store only the digest, so a leaked auth file cannot be replayed
        #[arg(long)]
        hashed: bool,
        /// Paths this token may write, comma separated globs. Empty means every path
        #[arg(long)]
        scope: Option<String>,
        /// Branches this token may write, comma separated globs. Empty means every branch
        #[arg(long)]
        branches: Option<String>,
    },
    /// Replace a token's secret, keeping its role, scope and name
    Rotate {
        /// Name of the token, or the last four characters `token list` shows
        selector: String,
        /// Store only the digest of the new secret
        #[arg(long)]
        hashed: bool,
    },
    /// List configured tokens without revealing them
    List,
    /// Erase a plaintext remote token a store built before #201 still carries
    Clear {
        /// Remote whose stored token should be erased
        name: String,
    },
    /// Show what the server decided: allowed, refused, and why
    Audit {
        /// Only the requests that were turned away
        #[arg(long)]
        refused: bool,
        /// Show only the last N events
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Remove a token by name or by its last four characters
    Remove { selector: String },
}

#[derive(Subcommand)]
enum IgnoreCmd {
    /// Add a pattern
    Add { pattern: String },
    /// List patterns
    List,
    /// Remove a pattern
    Remove { pattern: String },
    /// Say which attribute rule decides how a path is stored and checked out
    Attr {
        /// Path relative to the repository root
        path: String,
        /// Machine readable
        #[arg(long = "json")]
        json_attr: bool,
    },
    /// Say whether a path is ignored, and which rule decides it
    Why {
        path: String,
        /// Answer as JSON
        #[arg(long)]
        json_why: bool,
    },
}

/// Exit quietly when the reader closed the pipe.
///
/// `println!` panics on a broken pipe, so `gesta status | head -1` printed a
/// panic message and a backtrace from inside the standard library. Every tool
/// that writes to stdout is piped into something eventually, and git exits
/// silently in the same situation, so a closed pipe is normal use rather than an
/// error worth a stack trace.
///
/// Only a broken pipe is swallowed. Any other panic keeps the default message,
/// because hiding a real bug would be worse than the noise.
/// Escape a message for the one-line JSON error, so a quote or a newline in a path does not
/// produce a broken object.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn quiet_broken_pipe() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let text = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if text.contains("Broken pipe") {
            std::process::exit(0);
        }
        default(info);
    }));
}

fn main() {
    quiet_broken_pipe();
    if let Err(e) = run() {
        // The code decides the exit status, so a caller can tell "nothing there" from "the store
        // is damaged" without reading English. The message still goes to a person unchanged.
        let code = error::code_of(e.as_ref());
        // With --json the failure goes to stdout as one object, so a caller that asked for JSON
        // does not have to parse English out of stderr. Without it the sentence is unchanged.
        // Only when nothing was printed yet. `merge --json` prints the conflicted files and then
        // returns an error to keep the exit code non-zero; a second object after that document
        // makes the output unparseable, which is worse than having no error object at all.
        if cli::JSON_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
            && !cli::JSON_PRINTED.load(std::sync::atomic::Ordering::Relaxed)
        {
            if let Some(ours) = e.downcast_ref::<error::Error>() {
                println!("{}", ours.json());
            } else {
                println!(
                    "{{\"schema\":\"{}\",\"error\":{{\"code\":\"{}\",\"message\":\"{}\",\"advice\":\"{}\"}}}}",
                    cli::SCHEMA_VERSION,
                    code.name(),
                    escape_json(&e.to_string()),
                    escape_json(code.advice())
                );
            }
        }
        eprintln!("gesta: {}", e);
        std::process::exit(match code {
            error::Code::NotFound | error::Code::PathNotFound => 3,
            error::Code::Usage => 2,
            error::Code::Auth => 4,
            error::Code::DirtyWorktree | error::Code::Unmerged => 5,
            _ => 1,
        });
    }
}

/// One example of a command with its arguments filled in, for the commands an agent is most likely
/// to call with one missing.
///
/// A table rather than deriving the name from the usage line: `gesta changeset create <MESSAGE>`
/// would become `gesta changeset create MESSAGE`, which looks like a real command and fails if
/// pasted. An example has to be a value someone could actually use.
///
/// Returning None is not a failure: clap's own message already prints the usage line, so a command
/// absent from this table is no worse off than before.
fn example_for(argv: &[String]) -> Option<&'static str> {
    let words: Vec<&str> = argv
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(|a| a.as_str())
        .collect();
    let key = words.join(" ");
    // Longest match first, so `changeset create` wins over `changeset`.
    let table: &[(&str, &str)] = &[
        (
            "changeset create",
            "gesta changeset create \"what changed, in one line\"",
        ),
        ("changeset apply", "gesta changeset apply cs1234"),
        ("changeset diff", "gesta changeset diff cs1234"),
        ("changeset sign", "gesta changeset sign cs1234"),
        (
            "changeset approve",
            "gesta changeset approve cs1234 --by alice",
        ),
        ("changeset approvals", "gesta changeset approvals cs1234"),
        ("changeset find", "gesta changeset find --commit 3f2a1c9"),
        ("branch create", "gesta branch create feature"),
        ("branch switch", "gesta branch switch feature"),
        ("branch delete", "gesta branch delete feature"),
        ("branch info", "gesta branch info main"),
        ("branch merge", "gesta branch merge feature --target main"),
        ("branch protect", "gesta branch protect main"),
        ("branch conflicts", "gesta branch conflicts feature"),
        (
            "remote add",
            "gesta remote add origin https://example.com/repo.git",
        ),
        (
            "remote clone",
            "gesta remote clone https://example.com/repo.git",
        ),
        (
            "forge push",
            "gesta forge push https://github.com/owner/repo main",
        ),
        ("secret set", "gesta secret set s3.access_key"),
        ("secret remove", "gesta secret remove s3.access_key"),
        ("sub add", "gesta sub add vendor/lib"),
        ("sub pin", "gesta sub pin vendor/lib"),
        ("sub remove", "gesta sub remove vendor/lib"),
        ("hook install", "gesta hook install pre-commit"),
        ("hook remove", "gesta hook remove pre-commit"),
        ("ignore add", "gesta ignore add target/"),
        ("ignore remove", "gesta ignore remove target/"),
        ("ignore attr", "gesta ignore attr src/main.rs"),
        ("ignore why", "gesta ignore why target/debug/app"),
        (
            "submodule add",
            "gesta submodule add vendor/lib https://example.com/lib.git",
        ),
        ("notes add", "gesta notes add cs1234 --message \"why\""),
        ("notes show", "gesta notes show cs1234"),
        ("notes remove", "gesta notes remove cs1234"),
        ("worktree add", "gesta worktree add ../other feature"),
        ("worktree remove", "gesta worktree remove ../other"),
        ("blame", "gesta blame src/main.rs"),
        ("history", "gesta history src/main.rs"),
        ("show", "gesta show main:src/main.rs"),
        ("grep", "gesta grep TODO --path src"),
        ("backup", "gesta backup repo.gesta"),
        ("verify-backup", "gesta verify-backup repo.gesta"),
        ("restore-repo", "gesta restore-repo repo.gesta"),
        ("cherry-pick", "gesta cherry-pick cs1234"),
        ("revert", "gesta revert cs1234"),
        ("land", "gesta land feature --onto main"),
        ("landed", "gesta landed cs1234"),
        ("tag", "gesta tag v1.0.0"),
        (
            "git export",
            "gesta git export https://example.com/upstream.git",
        ),
        (
            "git push",
            "gesta git push https://example.com/upstream.git",
        ),
        (
            "git pull",
            "gesta git pull https://example.com/upstream.git",
        ),
        (
            "backup-remote pull",
            "gesta backup-remote pull s3://bucket/key",
        ),
        ("bisect start", "gesta bisect start cs1234 cs5678"),
        ("bisect run", "gesta bisect run -- cargo test"),
    ];
    // The longest key that the argv starts with, so a subcommand beats its parent.
    table
        .iter()
        .filter(|(k, _)| key == *k || key.starts_with(&format!("{} ", k)))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, v)| *v)
}

fn run() -> ZResult<()> {
    // `try_parse` rather than `parse`, so the missing-argument message can carry one line the
    // parser does not write: what the command is for and an example of it filled in. clap already
    // prints `Usage: gesta changeset create <MESSAGE>`, which names the shape to a reader who knows
    // how to read a usage line and says nothing to a reader who does not.
    //
    // Handled here rather than by declaring 37 arguments optional across nine files: every field
    // that came up would have changed a function signature, and the whole point of the message is
    // one sentence in one place. The commands whose argument can be inferred (the only pending
    // changeset, the current branch) keep their hand-written refusals, because those do more than
    // explain: they resolve.
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let text = e.to_string();
            if text.contains("required arguments were not provided") {
                if let Some(hint) = example_for(&std::env::args().collect::<Vec<_>>()) {
                    eprint!("{}", text);
                    eprintln!("\n{}", hint);
                    std::process::exit(2);
                }
            }
            // Every other parse outcome, including --help and --version, is clap's to print and clap
            // prints it: `e.exit()` writes to the right stream and uses the right status.
            e.exit()
        }
    };
    let json = cli.json;
    cli::JSON_REQUESTED.store(json, std::sync::atomic::Ordering::Relaxed);
    match cli.cmd {
        Cmd::Status if json => cli::cmd_status_json(),
        Cmd::Log {
            limit, envelope, ..
        } if json => cli::cmd_log_json(limit, envelope),
        Cmd::Blame { file, ignore_rev } if json => {
            let file = file
                .ok_or_else(|| bad("blame needs a file: `gesta blame src/main.rs`".to_string()))?;
            cli::cmd_blame_json(&file, &ignore_rev)
        }
        Cmd::History { file } if json => {
            let file = file.ok_or_else(|| {
                bad("history needs a file: `gesta history src/main.rs`".to_string())
            })?;
            cli::cmd_history_json(&file)
        }
        Cmd::Diff {
            changeset,
            from,
            to,
            name_only,
        } if json => {
            if from.is_some() || to.is_some() {
                cli::cmd_diff_range_json(from.as_deref(), to.as_deref(), name_only)
            } else {
                cli::cmd_diff_json(changeset.as_deref(), name_only)
            }
        }
        Cmd::Tag {
            name,
            changeset,
            list,
            force,
        } if json => cli::cmd_tag_json(name.as_deref(), changeset.as_deref(), list, force),
        Cmd::Ops { limit } if json => cli::cmd_ops_json(limit),
        // The specific arms come before the general one: a catch-all
        // `Cmd::Branch { sub } if json` would swallow them, and a match arm
        // that never runs is worse than a compile error.
        Cmd::Branch {
            sub: BranchCmd::Conflicts { source, target },
        } => cli::cmd_merge_preview(&source, &target),
        Cmd::Branch {
            sub:
                BranchCmd::Merge {
                    source,
                    target,
                    author,
                },
        } if json => {
            let source = source.ok_or_else(|| {
                bad("branch merge needs the branch to merge: `gesta branch merge feature --target main`"
                    .to_string())
            })?;
            cli::cmd_branch_merge_json(&source, &target, &author)
        }
        Cmd::Branch { sub } if json => cli::cmd_branch_json(sub),
        Cmd::Changeset { sub } if json => cli::cmd_changeset_json(sub),
        Cmd::Notes { sub } => cli::cmd_notes(sub),
        Cmd::Worktree { sub } => cli::cmd_worktree(sub),
        Cmd::Sparse { sub } => cli::cmd_sparse(sub),
        Cmd::Rebase {
            onto,
            branch,
            author,
            continue_,
            abort,
        } => cli::cmd_rebase(
            onto.as_deref(),
            branch.as_deref(),
            &author,
            continue_,
            abort,
        ),
        Cmd::CherryPick {
            source,
            onto,
            author,
        } => {
            let source = source.ok_or_else(|| {
                bad(
                    "cherry-pick needs the changeset to apply: `gesta cherry-pick cs1234`"
                        .to_string(),
                )
            })?;
            cli::cmd_cherry_pick(&source, onto.as_deref(), &author)
        }
        Cmd::Bisect { sub } => cli::cmd_bisect(sub),
        Cmd::Sub { sub } => cli::cmd_sub(sub, json),
        Cmd::Submodule { sub } => cli::cmd_submodule(sub, json),
        Cmd::Stash { sub } => cli::cmd_stash(sub, json),
        Cmd::Hook { sub } => cli::cmd_hook(sub, json),
        Cmd::Init { bare } => cli::cmd_init(bare),
        Cmd::Anonymise {
            authors,
            to,
            branch,
            dry_run,
        } => cli::cmd_anonymise(&authors, &to, &branch, dry_run),
        Cmd::Status => cli::cmd_status(),
        Cmd::Diff {
            changeset,
            from,
            to,
            name_only,
        } => {
            if from.is_some() || to.is_some() {
                cli::cmd_diff_range(from.as_deref(), to.as_deref(), name_only)
            } else {
                cli::cmd_diff(changeset.as_deref(), name_only)
            }
        }
        Cmd::Log { limit, oneline, .. } => cli::cmd_log(limit, oneline),
        Cmd::Changeset { sub } => match sub {
            ChangesetCmd::Sign { id, key } => cli::cmd_sign(&id, key.as_deref()),
            ChangesetCmd::Approve { id, by } => cli::cmd_approve(&id, &by),
            ChangesetCmd::Approvals { id } => cli::cmd_approvals(&id),
            ChangesetCmd::Find { commit } => cli::cmd_changeset_for_commit(&commit),
            ChangesetCmd::Create {
                message,
                author,
                paths,
                session,
                intent,
            } => {
                let session = session.or_else(|| std::env::var("GESTA_SESSION").ok());
                cli::cmd_changeset_create_paths(
                    &message,
                    &author,
                    session.as_deref(),
                    intent.as_deref(),
                    &paths,
                )
            }
            ChangesetCmd::Commit { id } => cli::cmd_changeset_commit(id.as_deref()),
            ChangesetCmd::Apply { id } => cli::cmd_changeset_apply(&id),
            ChangesetCmd::CommitAll => cli::cmd_changeset_commit_all(),
            ChangesetCmd::Diff { id } => cli::cmd_changeset_diff(&id),
            ChangesetCmd::List => cli::cmd_changeset_list(),
        },
        Cmd::Blame { file, ignore_rev } => {
            let file = file
                .ok_or_else(|| bad("blame needs a file: `gesta blame src/main.rs`".to_string()))?;
            cli::cmd_blame(&file, &ignore_rev)
        }
        Cmd::History { file } => {
            let file = file.ok_or_else(|| {
                bad("history needs a file: `gesta history src/main.rs`".to_string())
            })?;
            cli::cmd_history(&file)
        }
        Cmd::Show { spec, json } => {
            let spec = spec.ok_or_else(|| {
                bad("show needs a path, or `rev:path`: `gesta show main:src/main.rs`".to_string())
            })?;
            cli::cmd_show(&spec, json)
        }
        Cmd::Ls { rev, path, long: _ } if json => cli::cmd_ls_json(rev.as_deref(), path.as_deref()),
        Cmd::Ls { rev, path, long } => cli::cmd_ls(rev.as_deref(), path.as_deref(), long),
        Cmd::Git { sub } => match sub {
            GitCmd::Import { path, path_prefix } => {
                cli::cmd_git_import(&path, path_prefix.as_deref())
            }
            GitCmd::Export { path, branch } => cli::cmd_git_export(&path, branch.as_deref()),
            GitCmd::Push { remote, branch } => cli::cmd_git_push(&remote, branch.as_deref()),
            GitCmd::Pull {
                remote,
                path_prefix,
            } => cli::cmd_git_pull(&remote, path_prefix.as_deref()),
        },
        Cmd::Brief {
            json: brief_json,
            fix,
            stash,
        } => {
            if fix {
                cli::cmd_brief_fix(brief_json, stash)
            } else if brief_json {
                cli::cmd_brief_json()
            } else {
                cli::cmd_brief()
            }
        }
        Cmd::Grep {
            pattern,
            history,
            ignore_case,
            word,
            regex,
            path,
            limit,
            files_with_matches,
            max_line,
        } => {
            let pattern = pattern.ok_or_else(|| {
                bad("grep needs text to find: `gesta grep TODO --path src`".to_string())
            })?;
            let search = cli::Search {
                pattern,
                history,
                ignore_case,
                word,
                regex,
                path,
                limit,
                files_with_matches,
                max_line,
            };
            if json {
                cli::cmd_grep_json(search)
            } else {
                cli::cmd_grep(search)
            }
        }
        Cmd::Checkout { file, changeset } => {
            cli::cmd_checkout(file.as_deref(), changeset.as_deref())
        }
        Cmd::Restore { file } => cli::cmd_restore(file.as_deref()),
        Cmd::Backup { dest, json_backup } => {
            let dest = dest.ok_or_else(|| {
                bad("backup needs a destination file: `gesta backup repo.gesta`".to_string())
            })?;
            cli::cmd_backup(&dest, json_backup)
        }
        Cmd::BackupRemote { sub } => cli::cmd_backup_remote(sub),
        Cmd::VerifyBackup { file } => {
            let file = file.ok_or_else(|| {
                bad(
                    "verify-backup needs the backup file: `gesta verify-backup repo.gesta`"
                        .to_string(),
                )
            })?;
            cli::cmd_verify_backup(&file)
        }
        Cmd::RestoreRepo { file, yes } => {
            let file = file.ok_or_else(|| {
                bad(
                    "restore-repo needs the backup file: `gesta restore-repo repo.gesta`"
                        .to_string(),
                )
            })?;
            cli::cmd_restore_repo(&file, yes)
        }
        Cmd::Undo => cli::cmd_undo(),
        Cmd::Redo => cli::cmd_redo(),
        Cmd::Revert { id, reason, author } => {
            let id = id.ok_or_else(|| {
                bad("revert needs the changeset to undo: `gesta revert cs1234`".to_string())
            })?;
            cli::cmd_revert(&id, reason.as_deref(), &author)
        }
        Cmd::Ops { limit } => cli::cmd_ops(limit),
        Cmd::Tag {
            name,
            changeset,
            list,
            force,
        } => cli::cmd_tag(name.as_deref(), changeset.as_deref(), list, force),
        // The text path goes through the same dispatch as everything else: the match that used to
        // live here duplicated `dispatch_branch_text` arm for arm, so a change had to be made in
        // two places or one of them silently kept the old behaviour.
        Cmd::Branch { sub } => cli::dispatch_branch_text(sub),
        Cmd::Forge { sub } => match sub {
            ForgeCmd::Push {
                url,
                branch,
                remote_ref,
                all,
                token,
                user,
                json,
            } => {
                // The token comes from GESTA_FORGE_TOKEN or the terminal. It stays reachable as an
                // argument only because a non-interactive caller has no terminal, and taking it that
                // way is warned about: the flag's own documentation says "a token in an argument is a
                // token in the shell history and in `ps`".
                let token = cli::secret_not_in_argv(token, "GESTA_FORGE_TOKEN", "token: ", true)
                    .map_err(|e| e.to_string())?;
                if all {
                    cli::cmd_forge_push_all(&url, Some(&token), &user, json)
                } else {
                    cli::cmd_forge_push(&url, &branch, &remote_ref, Some(&token), &user, json)
                }
            }
        },
        Cmd::Remote { sub } => match sub {
            RemoteCmd::Add {
                name,
                url,
                token,
                pin,
            } => {
                let token = match token {
                    Some(_) => Some(
                        cli::secret_not_in_argv(token, "GESTA_REMOTE_TOKEN", "token: ", true)
                            .map_err(|e| e.to_string())?,
                    ),
                    None => None,
                };
                cli_remote::cmd_remote_add(&name, &url, token.as_deref(), pin.as_deref())
            }
            RemoteCmd::List => cli_remote::cmd_remote_list(),
            RemoteCmd::Token { sub } => match sub {
                TokenCmd::Add {
                    role,
                    name,
                    hashed,
                    scope,
                    branches,
                } => cli_remote::cmd_token_add(
                    &role,
                    &name,
                    hashed,
                    scope.as_deref(),
                    branches.as_deref(),
                ),
                TokenCmd::Rotate { selector, hashed } => {
                    cli_remote::cmd_token_rotate(&selector, hashed)
                }
                TokenCmd::List => cli_remote::cmd_token_list(),
                TokenCmd::Clear { name } => cli_remote::cmd_remote_token_clear(&name),
                TokenCmd::Remove { selector } => cli_remote::cmd_token_remove(&selector),
                TokenCmd::Audit { refused, limit } => cli_remote::cmd_audit(refused, limit, json),
            },
            RemoteCmd::Ls { remote } => cli_remote::cmd_remote_ls(&remote),
            RemoteCmd::Deepen {
                depth,
                unshallow,
                remote,
                branch,
                dry_run,
            } => cli_remote::cmd_deepen(depth, unshallow, &remote, &branch, dry_run),
            RemoteCmd::Inspect {
                remote,
                branch,
                json_inspect,
                check_identity,
            } => cli_remote::cmd_inspect(&remote, &branch, json_inspect || json, check_identity),
            RemoteCmd::Push { remote, branch } => cli_remote::cmd_push(&remote, &branch),
            RemoteCmd::Pull {
                remote,
                branch,
                author,
            } => cli_remote::cmd_pull(&remote, &branch, &author),
            RemoteCmd::Clone {
                url,
                dir,
                token,
                pin,
                depth,
            } => {
                let token = match token {
                    Some(_) => Some(
                        cli::secret_not_in_argv(token, "GESTA_REMOTE_TOKEN", "token: ", true)
                            .map_err(|e| e.to_string())?,
                    ),
                    None => None,
                };
                cli_remote::cmd_clone(&url, &dir, token.as_deref(), pin.as_deref(), depth)
            }
        },
        Cmd::Secret { sub } => cli::cmd_secret(sub),
        Cmd::Land {
            source,
            onto,
            expect,
            author,
        } => {
            let source = source.ok_or_else(|| {
                bad("land needs the branch to land: `gesta land feature --onto main`".to_string())
            })?;
            cli::cmd_land(&source, &onto, expect.as_deref(), &author)
        }
        Cmd::Landed {
            changeset,
            branch,
            json,
        } => {
            let changeset = changeset.ok_or_else(|| {
                bad("landed needs the changeset to look for: `gesta landed cs1234`".to_string())
            })?;
            cli::cmd_landed(&changeset, &branch, json)
        }
        Cmd::Review { from, to, min_risk } => {
            if json {
                cli::cmd_review_json(from.as_deref(), to.as_deref(), min_risk.as_deref())
            } else {
                cli::cmd_review(from.as_deref(), to.as_deref(), min_risk.as_deref())
            }
        }
        Cmd::Mcp => cli::cmd_mcp(),
        #[cfg(windows)]
        Cmd::Mount { target, branch } => cli::cmd_mount(&target, branch.as_deref()),
        Cmd::Serve {
            bind,
            port,
            tls,
            key,
        } => cli_remote::cmd_serve(&bind, port, tls.as_deref(), key.as_deref()),
        Cmd::Fsck { prune, worktree } => cli_remote::cmd_fsck(prune, worktree),
        Cmd::Gc => cli_remote::cmd_gc(),
        Cmd::Key { sub } => match sub {
            KeyCmd::Generate { path } => cli::cmd_key_generate(path.as_deref()),
            KeyCmd::Rotate { path, dry_run } => cli::cmd_key_rotate(path.as_deref(), dry_run),
            KeyCmd::List => cli::cmd_key_list(),
        },
        Cmd::Verify { id, key } => cli::cmd_verify(id.as_deref(), key.as_deref()),
        Cmd::Stats => cli_remote::cmd_stats(),
        Cmd::Ignore { sub } => match sub {
            IgnoreCmd::Add { pattern } => cli_remote::cmd_ignore_add(&pattern),
            IgnoreCmd::List => cli_remote::cmd_ignore_list(),
            IgnoreCmd::Remove { pattern } => cli_remote::cmd_ignore_remove(&pattern),
            IgnoreCmd::Why { path, json_why } => {
                cli_remote::cmd_ignore_why(&path, json_why || json)
            }
            IgnoreCmd::Attr { path, json_attr } => {
                cli_remote::cmd_attributes_why(&path, json_attr || json)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subcommand_example_beats_its_parent() {
        // A lookup keyed on the words before the first flag. `changeset create` and `changeset` both
        // match `gesta changeset create`, and the longer one is the example a reader needs: the
        // parent's would print a valid command that does not answer their error.
        let argv: Vec<String> = ["gesta", "changeset", "create"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            example_for(&argv),
            Some("gesta changeset create \"what changed, in one line\"")
        );
    }

    #[test]
    fn a_flag_before_the_argument_does_not_change_the_match() {
        // Flags are dropped before the lookup, so the same example is found whether the caller wrote
        // `--by alice` before or after the missing argument. A table keyed on raw argv would miss
        // this and fall through to the parent's example or to none.
        let argv: Vec<String> = ["gesta", "changeset", "approve", "--by", "alice"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            example_for(&argv),
            Some("gesta changeset approve cs1234 --by alice")
        );
    }

    #[test]
    fn a_command_with_no_example_returns_none_rather_than_a_wrong_one() {
        // clap already prints the usage line, so returning nothing leaves the caller no worse off.
        // Returning a fabricated example would be worse: it would look runnable and fail.
        let argv: Vec<String> = ["gesta", "fsck", "nonsense"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(example_for(&argv), None);
    }

    #[test]
    fn every_example_in_the_table_names_the_command_it_is_filed_under() {
        // The table is keyed by hand, so a typo in the key or the value is invisible until someone
        // reads it at the moment they are already stuck. This walks the same table through the same
        // lookup, so a row that cannot be found by its own key fails here.
        let keys = [
            "changeset create",
            "changeset apply",
            "changeset diff",
            "changeset sign",
            "changeset approve",
            "changeset approvals",
            "changeset find",
            "branch create",
            "branch switch",
            "branch delete",
            "branch info",
            "branch merge",
            "branch protect",
            "branch conflicts",
            "remote add",
            "remote clone",
            "forge push",
            "secret set",
            "secret remove",
            "sub add",
            "sub pin",
            "sub remove",
            "hook install",
            "hook remove",
            "ignore add",
            "ignore remove",
            "ignore attr",
            "ignore why",
            "submodule add",
            "notes add",
            "notes show",
            "notes remove",
            "worktree add",
            "worktree remove",
            "blame",
            "history",
            "show",
            "grep",
            "backup",
            "verify-backup",
            "restore-repo",
            "cherry-pick",
            "revert",
            "land",
            "landed",
            "tag",
            "git export",
            "git push",
            "git pull",
            "backup-remote pull",
            "bisect start",
            "bisect run",
        ];
        let mut broken = Vec::new();
        for key in keys {
            let mut argv = vec!["gesta".to_string()];
            argv.extend(key.split(' ').map(|s| s.to_string()));
            match example_for(&argv) {
                Some(example) => {
                    // The example must be the command it is filed under, with arguments added.
                    // Anything else means the row was filed under the wrong key.
                    let prefix = format!("gesta {key}");
                    if !example.starts_with(&prefix) {
                        broken.push(format!("{key} -> {example}"));
                    }
                }
                None => broken.push(format!("{key} -> (none)")),
            }
        }
        assert!(
            broken.is_empty(),
            "examples that do not name their own command: {:?}",
            broken
        );
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gesta-root-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_new_repository_uses_the_current_directory_name() {
        let d = tmp("new");
        std::fs::create_dir_all(d.join(ZF_DIR)).unwrap();
        assert_eq!(repo_dir(&d), d.join(ZF_DIR));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_repository_made_before_the_rename_is_still_found() {
        // The rename touched the directory name and the database file name at
        // once. Finding the directory but not the database created an empty one
        // beside the real one, and the repository then looked empty.
        let d = tmp("old");
        std::fs::create_dir_all(d.join(LEGACY_DIR)).unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(LEGACY_DB), b"not empty").unwrap();

        assert_eq!(repo_dir(&d), d.join(LEGACY_DIR));
        assert_eq!(repo_db(&d), d.join(LEGACY_DIR).join(LEGACY_DB));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_database_of_the_new_name_does_not_shadow_the_real_one() {
        // An interrupted start can leave a zero byte file behind. Choosing it
        // because it exists is how a full repository reads as empty.
        let d = tmp("shadow");
        std::fs::create_dir_all(d.join(LEGACY_DIR)).unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(LEGACY_DB), b"schema and data").unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(ZF_DB), b"").unwrap();

        assert_eq!(repo_db(&d), d.join(LEGACY_DIR).join(LEGACY_DB));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_new_name_wins_when_both_exist_with_content() {
        let d = tmp("both");
        std::fs::create_dir_all(d.join(ZF_DIR)).unwrap();
        std::fs::create_dir_all(d.join(LEGACY_DIR)).unwrap();
        std::fs::write(d.join(ZF_DIR).join(ZF_DB), b"new data").unwrap();
        std::fs::write(d.join(LEGACY_DIR).join(LEGACY_DB), b"old data").unwrap();

        assert_eq!(repo_dir(&d), d.join(ZF_DIR));
        assert_eq!(repo_db(&d), d.join(ZF_DIR).join(ZF_DB));
        let _ = std::fs::remove_dir_all(&d);
    }
}
