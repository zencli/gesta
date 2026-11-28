//! The store tier.
//! SQLite storage: schema, compressed object store, refs, graph queries.

pub mod cache;
pub mod content;
pub mod delta;
pub mod delta_pack;
pub mod gitobj;
pub mod pack;
pub mod packfile;
pub mod sidecar;
pub mod sparse;
#[cfg(windows)]
pub mod vfs;
pub mod walk;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::{bad, now, repo_db, ZResult};

pub fn db_path(root: &Path) -> PathBuf {
    repo_db(root)
}

/// The on-disk shape this build expects.
///
/// Bump this whenever a table, column or index is added to `init_schema` or `migrate`. A database
/// stamped with an older value runs the migration body once; one stamped with this value skips it.
/// Forgetting to bump it does not corrupt anything, but a repository created by an older build keeps
/// paying the migration cost on every open.
const SCHEMA_VERSION: i64 = 1;

pub fn open_db(root: &Path) -> ZResult<Connection> {
    let conn = Connection::open(db_path(root))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=30000;",
    )?;
    Ok(conn)
}

/// The repository root, derived from the database file a connection was opened on.
///
/// The path is `<root>/.gesta/gesta.db`, so the grandparent of the file is the root. A connection that
/// was never named a file, as in the unit tests' in-memory database, has no root and returns `None`,
/// which callers treat as "keep the object inline".
fn db_root(conn: &Connection) -> Option<std::path::PathBuf> {
    let path = conn.path()?;
    let dir = std::path::Path::new(path).parent()?; // .gesta
    dir.parent().map(|p| p.to_path_buf()) // the repository
}

/// Is this error SQLite refusing a write because another connection holds the
/// write lock?
pub fn is_busy(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => {
            matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        }
        _ => false,
    }
}

/// Run `f`, retrying while SQLite reports the database as busy.
///
/// `busy_timeout` covers a reader waiting for a writer. It does not cover two
/// writers arriving at once from different processes: SQLite returns SQLITE_BUSY
/// immediately for a write-write conflict when a transaction has already started
/// reading. Retrying with a growing pause is what makes concurrent commits work;
/// without it, 20 parallel commits produced 19 failures.
pub fn with_retry<T>(mut f: impl FnMut() -> ZResult<T>) -> ZResult<T> {
    let mut paused = std::time::Duration::from_millis(5);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let busy = e
                    .downcast_ref::<rusqlite::Error>()
                    .map(is_busy)
                    .unwrap_or(false);
                if !busy || std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(paused);
                paused = (paused * 2).min(std::time::Duration::from_millis(250));
            }
        }
    }
}

/// Add a column to a table that predates it.
///
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so the column list decides. An
/// existing repository keeps working without a manual migration step.
/// Add a column if the table does not have it. Returns true when this call added it, which is the
/// only moment a caller can know the column is empty and needs filling.
/// Columns of a table, read once per process.
///
/// `PRAGMA table_info` is not free on a table with rows in it: SQLite opens the schema and walks the
/// column list, and `migrate` used to ask for the same table's columns once per column it checked -
/// five times for `changesets` alone. On a repository with a thousand changesets that was thirty
/// milliseconds of every command, which is the whole reason `bench/long_history.py` read a slope: it
/// was timing the schema check, not the commit.
///
/// Cached by (database path, table). Keying on the table alone is only safe while one process opens
/// one database, and that is not a rule anything enforces: the server opens a connection per
/// request, and a caller that ever opens two repositories would find a column present in the first
/// suppressing the ALTER in the second, then fail with "no such column" on a query that should work.
///
/// The cache lives at module scope so `invalidate_columns` can reach the same map. It used to be a
/// `thread_local` declared inside this function, and `invalidate_columns` declared its OWN
/// `thread_local` of a DIFFERENT type (`HashMap<String, Vec<String>>` against
/// `HashMap<(String, String), Vec<String>>`) - so its `remove` deleted from a map nobody ever wrote
/// or read, and the real cache was never cleared (#308). `ensure_column` then looked at a stale list,
/// tried the `ALTER` again, and got "duplicate column", which it swallowed as success.
mod columns_cache {
    use std::cell::RefCell;
    thread_local! {
        /// Column lists, keyed by (database path, table).
        ///
        /// Kept here rather than inside `columns_cached` so `invalidate_columns` can reach it. It
        /// used to be a `thread_local` inside that function, and `invalidate_columns` declared its own
        /// of a DIFFERENT type (`HashMap<String, Vec<String>>` against
        /// `HashMap<(String, String), Vec<String>>`), so its `remove` deleted from a map nobody wrote
        /// and the real cache was never cleared (#308). `ensure_column` then re-read a stale list,
        /// tried the `ALTER` again, and swallowed the resulting "duplicate column" as success.
        pub static CACHE: RefCell<std::collections::HashMap<(String, String), Vec<String>>> =
            RefCell::new(std::collections::HashMap::new());
    }
}

fn columns_cached(conn: &Connection, table: &str) -> ZResult<Vec<String>> {
    // The database path is read per call and not cached against the connection.
    //
    // It used to be cached by the connection's address, only inserted and never removed. A closed
    // connection's address is handed to the next one, so a new connection on a different file read
    // the previous path and was then served another database's column list (#308). There is no
    // teardown hook to hang a cleanup on - `Connection` is rusqlite's, with no `Drop` of ours to
    // extend - so the safe answer is not to keep the mapping at all.
    //
    // Measured on a warm in-memory handle: `PRAGMA database_list` costs 1.29 microseconds, and this
    // runs once per (connection, table) rather than once per column, because the result is keyed into
    // CACHE below. That is cheaper than the bug it prevents.
    let db: String = conn
        .query_row("PRAGMA database_list", [], |r| {
            r.get::<_, Option<String>>(2)
        })
        .unwrap_or(None)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ":memory:".to_string());
    let key = (db, table.to_string());
    if let Some(hit) = columns_cache::CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(hit);
    }
    let cols = table_columns(conn, table)?;
    columns_cache::CACHE.with(|c| c.borrow_mut().insert(key, cols.clone()));
    Ok(cols)
}

fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> ZResult<bool> {
    let existing = columns_cached(conn, table)?;
    if !existing.iter().any(|c| c == column) {
        // The check above and the ALTER below are not one atomic step. Twenty processes starting on a
        // fresh repository all find the column missing and all try to add it; one wins and the rest
        // get "duplicate column name". That is the migration having already happened elsewhere, which
        // is the outcome the caller wanted, so it is success rather than an error.
        //
        // Found by `bench/concurrency.py`, which failed about one run in eight with exactly this
        // message while every other check passed.
        match conn.execute(
            &format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, decl),
            [],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(ref m)))
                if m.contains("duplicate column") => {}
            Err(e) => return Err(e.into()),
        }
        // The cache said this column was missing and it has just been added, so the cached answer
        // would be wrong for every later check in this process.
        invalidate_columns(table);
        return Ok(true);
    }
    Ok(false)
}

/// Drop the cached column list for a table, in every database this thread has cached.
///
/// The table is the whole identity here: the caller has just changed that table's shape, and a
/// shape change applies to the table wherever it lives. Keeping the database in the filter would
/// mean a second repository opened in the same process never saw its own column being added.
fn invalidate_columns(table: &str) {
    columns_cache::CACHE.with(|c| {
        c.borrow_mut().retain(|(_, t), _| t != table);
    });
}

/// Give every committed changeset a generation number, oldest first.
///
/// A row's number is one more than its highest parent's, so a parent has to be filled before its
/// child. Walking the table in creation order is not enough: an import can land a child before its
/// parent, and a merge can reference a row that arrived later. This resolves each row after its
/// parents by working over the dependency order instead of the row order, and a row whose parents
/// are all resolved is settled in the same pass, so one pass settles a whole chain that arrived out
/// of order.
///
/// A parent that is not in this table at all (a shallow import whose base was never brought in)
/// counts as 0, which makes the child a root of what this repository actually holds. That is what a
/// shallow clone means.
fn backfill_generation(conn: &Connection) -> ZResult<()> {
    let mut stmt = conn.prepare("SELECT id, parent, parent2 FROM changesets")?;
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let present: HashSet<String> = rows.iter().map(|(id, _, _)| id.clone()).collect();
    let mut parents: std::collections::HashMap<String, (Option<String>, Option<String>)> = rows
        .into_iter()
        .map(|(id, p1, p2)| (id, (p1, p2)))
        .collect();
    let mut done: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

    // Resolve in rounds. Every round settles at least one row, or the loop stops with what is
    // left, which is a cycle - something a correct repository cannot have and a corrupt one can.
    loop {
        let ready: Vec<String> = parents
            .iter()
            .filter(|(id, _)| !done.contains_key(*id))
            .filter(|(_, (p1, p2))| {
                let ok = |p: &Option<String>| match p {
                    None => true,
                    Some(h) => !present.contains(h) || done.contains_key(h),
                };
                ok(p1) && ok(p2)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if ready.is_empty() {
            break;
        }
        for id in ready {
            let (p1, p2) = parents.remove(&id).unwrap_or((None, None));
            let gen = |p: &Option<String>| -> i64 {
                p.as_ref().and_then(|h| done.get(h).copied()).unwrap_or(0)
            };
            let g = 1 + gen(&p1).max(gen(&p2));
            conn.execute(
                "UPDATE changesets SET generation = ?2 WHERE id = ?1",
                params![id, g],
            )?;
            done.insert(id, g);
        }
    }
    Ok(())
}

pub fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS objects (
            hash       TEXT PRIMARY KEY,
            content    BLOB NOT NULL,
            size       INTEGER NOT NULL,
            compressed INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            -- Where the body actually lives when it shares a stream with other objects. NULL means
            -- it is in `content`, which is every object in a repository that never ran `gc --pack`.
            group_id   INTEGER,
            offset     INTEGER,
            length     INTEGER,
            -- 1 when the body lives beside the database instead of in `content`. A large object in a
            -- row is copied by every operation that touches the page, so past the threshold it is a
            -- file with a row that names it.
            sidecar    INTEGER NOT NULL DEFAULT 0,
            -- The base of a delta chain: the object this one is a delta against. NULL means stored whole.
            -- A chain is built when a new file shares a large prefix with an existing object; the newest
            -- object stores the delta and points to the base. A chain has no cycles because every base is
            -- older than the object that points to it, and the depth is capped by MAX_DELTA_DEPTH.
            base_hash  TEXT
        );
        -- One compressed stream holding several objects, so the compressor can see across them.
        -- A zlib stream cannot, and the shared vocabulary in source files is most of what it costs.
        CREATE TABLE IF NOT EXISTS object_groups (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            packed       BLOB NOT NULL,
            raw_size     INTEGER NOT NULL,
            object_count INTEGER NOT NULL,
            created_at   INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS changesets (
            id         TEXT PRIMARY KEY,
            message    TEXT NOT NULL,
            author     TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            status     TEXT NOT NULL CHECK (status IN ('pending','committed')),
            parent     TEXT,
            parent2    TEXT,
            branch     TEXT NOT NULL DEFAULT 'main',
            -- Signature over this changeset's identity, and the public key it verifies with.
            signature  TEXT,
            signer     TEXT,
            -- The author timezone a git commit was written in. git hashes the author line including
            -- the offset, so an export that writes a different one produces a different commit name
            -- for identical content. NULL means the changeset was made here, not imported.
            tz         TEXT,
            -- Provenance: which session produced this change and why. When an agent
            -- writes most of the code, the reason behind a change is part of the
            -- change; a message alone loses it.
            session    TEXT,
            intent     TEXT,
            -- The git commit this changeset was imported from, so a second import of the same
            -- repository recognises what is already here instead of duplicating it.
            git_commit TEXT,
            -- Longest distance to a root, so a changeset with no parent is 1 and every child is
            -- one more than its highest parent. Two changesets at the same generation cannot be
            -- ancestors of each other, which is what lets a reachability walk drop a whole level
            -- at once instead of walking it. git, Jujutsu, Sapling and Wren all keep this number
            -- for the same reason. NULL means the row predates the column and has not been
            -- backfilled; readers treat NULL as "unknown" and fall back to the plain walk.
            generation INTEGER
        );
        CREATE TABLE IF NOT EXISTS changeset_files (
            changeset_id TEXT NOT NULL,
            path         TEXT NOT NULL,
            old_hash     TEXT,
            new_hash     TEXT,
            -- The git mode this path carried: 100644, 100755 for a file with the executable bit, or
            -- 120000 for a symlink. Null means 100644, which is what every row written before the
            -- column existed is. Without it a round trip through git turns every executable into a
            -- plain file while all the bytes still match.
            mode         TEXT,
            PRIMARY KEY (changeset_id, path)
        );
        -- A git-lfs lock: one path held by one owner until it is released. git-lfs uses these so two
        -- people do not edit the same binary at once, and the format is a table rather than a file so
        -- that a lock survives a backup and a restore, which is what the drill in bench/backup_remote.py
        -- checks.
        CREATE TABLE IF NOT EXISTS lfs_locks (
            id         TEXT PRIMARY KEY,
            path       TEXT NOT NULL UNIQUE,
            owner      TEXT NOT NULL,
            refname    TEXT,
            locked_at  INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS file_versions (
            path         TEXT NOT NULL,
            version      INTEGER NOT NULL,
            hash         TEXT NOT NULL,
            changeset_id TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (path, version)
        );
        -- Blame is stored as ranges, not as one row per line.
        --
        -- A row per line repeats the changeset, the author and the timestamp for every line of
        -- every file. On this project that was 62,824 rows carrying one distinct changeset: 90% of
        -- the database held the same three values over and over, and the two indexes over them
        -- doubled it. A range says the same thing once: these lines, from this changeset.
        --
        -- `author` and `committed_at` are not stored here at all. They live on `changesets`, keyed
        -- by id, and a reader joins for them. Duplicating a value that has one home is how a
        -- database ends up mostly repetition.
        CREATE TABLE IF NOT EXISTS blame_ranges (
            path         TEXT NOT NULL,
            start_line   INTEGER NOT NULL,
            end_line     INTEGER NOT NULL,
            changeset_id TEXT NOT NULL,
            PRIMARY KEY (path, start_line)
        );
        CREATE INDEX IF NOT EXISTS idx_blame_ranges_cs ON blame_ranges(changeset_id);
        -- No working_copy table: committed content already lives in `objects`
        -- and duplicating it doubled disk usage for nothing.
        CREATE TABLE IF NOT EXISTS branches (
            name        TEXT PRIMARY KEY,
            head        TEXT,
            created_at  INTEGER NOT NULL,
            from_branch TEXT
        );
        -- A protected branch refuses a write that no approve token has signed off. The refusal lives
        -- beside the push rather than in a hook, because a hook is a program an operator has to
        -- install, and a gate nobody installs is not a gate.
        CREATE TABLE IF NOT EXISTS approvals (
            changeset_id TEXT NOT NULL,
            branch       TEXT NOT NULL,
            signer       TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (changeset_id, signer)
        );
        CREATE TABLE IF NOT EXISTS tags (
            name         TEXT PRIMARY KEY,
            changeset_id TEXT NOT NULL,
            message      TEXT,
            created_at   INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS ignore_rules (
            pattern TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS operations (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            parent_op  INTEGER,
            kind       TEXT NOT NULL,
            target     TEXT,
            detail     TEXT,
            snapshot   TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            -- Set when the operation has been undone. Keeping the row instead of deleting
            -- it is what makes undo reversible: a second undo walks further back, and a
            -- redo can step forward again.
            undone     INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS remotes (
            name  TEXT PRIMARY KEY,
            url   TEXT NOT NULL,
            token TEXT,
            pin   TEXT
        );
        CREATE TABLE IF NOT EXISTS remote_refs (
            remote       TEXT NOT NULL,
            branch       TEXT NOT NULL,
            changeset_id TEXT NOT NULL,
            updated_at   INTEGER NOT NULL,
            PRIMARY KEY (remote, branch)
        );
        CREATE TABLE IF NOT EXISTS deltas (
            hash       TEXT PRIMARY KEY,
            base       TEXT NOT NULL,
            payload    BLOB NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS worktree_cache (
            path  TEXT PRIMARY KEY,
            mtime INTEGER NOT NULL,
            size  INTEGER NOT NULL,
            hash  TEXT NOT NULL
        );
        -- Changesets whose parent was deliberately left out of a shallow clone. A
        -- row here turns "missing parent" into "boundary", which is the difference
        -- between a partial history and a damaged one.
        CREATE TABLE IF NOT EXISTS shallow_roots (
            changeset_id TEXT PRIMARY KEY,
            branch       TEXT NOT NULL,
            created_at   INTEGER NOT NULL
        );
        -- The head tree a shallow clone was given, so an object the working tree needs
        -- is not mistaken for an orphan. Without this, `fsck --prune` would delete
        -- files that are on disk and correct.
        CREATE TABLE IF NOT EXISTS snapshot_objects (
            path TEXT PRIMARY KEY,
            hash TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS repo_state (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_cf_changeset ON changeset_files(changeset_id);
        -- Which changesets touched a path. `rename_history` asks this to find the changeset that
        -- moved a file, and the table has one row per file per changeset, so without an index the
        -- question scans every file of every commit ever made. `EXPLAIN QUERY PLAN` said exactly
        -- that: `SCAN changeset_files`. The table's primary key is (changeset_id, path), so a
        -- lookup by path alone cannot use it.
        CREATE INDEX IF NOT EXISTS idx_cf_path ON changeset_files(path);
        -- The committed tree of each branch, kept up to date as changesets land.
        -- Rebuilding it from the whole history costs about 2 ms on a 2000 file
        -- repository and `status` needs it on every run. `fsck` compares this
        -- table against a rebuild, so a drift is detectable rather than silent.
        CREATE TABLE IF NOT EXISTS branch_tree (
            branch TEXT NOT NULL,
            path   TEXT NOT NULL,
            hash   TEXT NOT NULL,
            PRIMARY KEY (branch, path)
        );
        CREATE INDEX IF NOT EXISTS idx_fv_path ON file_versions(path);
        -- The latest version of a path, and the one before it, are asked for on every commit that
        -- touches the file. An index on `path` alone cannot answer `ORDER BY version DESC LIMIT 1`
        -- without reading every version of that path and sorting them, so the cost of a commit grew
        -- with the history of the file. Measured on a 10000 commit history: 11.6 ms per lookup,
        -- 10015 rows read, for the one row the caller asked for.
        CREATE INDEX IF NOT EXISTS idx_fv_path_version ON file_versions(path, version DESC);
        CREATE INDEX IF NOT EXISTS idx_cs_status ON changesets(status, created_at);
        CREATE INDEX IF NOT EXISTS idx_cs_parent ON changesets(parent);
        -- Choosing a delta base asks for the objects closest in size to a new one, so the
        -- range has to be an index lookup. Without this the query scans every object in the
        -- repository, which made the cost of a commit grow with the size of the store:
        -- measured at 1000 changesets, a commit went from 12.9 ms to 40.7 ms, 3.14x, and the
        -- gate's `long_history` step is what caught it.
        -- `(size, created_at)` serves both halves of the base search: the range on size and the
        -- recent-first order within it. `objects` is a WITHOUT ROWID table keyed by hash, so there
        -- is no rowid to order by; `created_at` is the column that says when the body was written.
        CREATE INDEX IF NOT EXISTS idx_objects_size_recent ON objects(size, created_at DESC);
        "#,
    )?;

    // Columns added after the first release. A repository created by an older
    // build has the table but not the column, so the schema step upgrades it in
    // place instead of asking the operator to run a migration.
    let _ = ensure_column(conn, "remotes", "pin", "TEXT")?;
    Ok(())
}

/// Upgrade a database created before v0.3 (uncompressed TEXT objects, no
/// operation log, no tags, no remotes). Returns true when it migrated.
pub fn migrate(conn: &Connection) -> ZResult<bool> {
    let obj_cols = table_columns(conn, "objects")?;
    if obj_cols.is_empty() {
        return Ok(false); // fresh database; init_schema creates the full shape
    }
    // The whole body below is idempotent DDL: 28 CREATE IF NOT EXISTS statements plus a dozen column
    // probes. Running it on every open is a fixed pile of write transactions, and a write transaction
    // is far more expensive on a large database with a dirty WAL than on a small one - so a store that
    // grew to 1000 commits paid 31 ms per open for work that changed nothing, against 6.7 ms at 100.
    // That read as "the cost of a commit grows with the history".
    //
    // user_version records the shape this code already produced. A database at SCHEMA_VERSION skips
    // the body; anything older runs it and is stamped.
    if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? >= SCHEMA_VERSION {
        return Ok(false);
    }
    let _t_idx = std::time::Instant::now();

    // Tables added after a database was created. Each is created once and then
    // filled from history, so an existing repository upgrades on first open
    // instead of failing with "no such table".
    let mut changed = false;
    for (table, ddl) in [
        (
            "branch_tree",
            "CREATE TABLE IF NOT EXISTS branch_tree (
                branch TEXT NOT NULL,
                path   TEXT NOT NULL,
                hash   TEXT NOT NULL,
                PRIMARY KEY (branch, path)
            )",
        ),
        (
            "stashes",
            "CREATE TABLE IF NOT EXISTS stashes (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                message    TEXT NOT NULL,
                author     TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
        ),
        (
            "subrepos",
            "CREATE TABLE IF NOT EXISTS subrepos (
                path       TEXT PRIMARY KEY,
                remote     TEXT NOT NULL,
                pinned     TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            )",
        ),
        (
            "bisect",
            "CREATE TABLE IF NOT EXISTS bisect (
                id         INTEGER PRIMARY KEY CHECK (id = 1),
                good       TEXT NOT NULL,
                bad        TEXT NOT NULL,
                candidates TEXT NOT NULL,
                current    TEXT,
                steps      INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            )",
        ),
        (
            "rebase",
            "CREATE TABLE IF NOT EXISTS rebase (
                id            INTEGER PRIMARY KEY CHECK (id = 1),
                branch        TEXT NOT NULL,
                onto          TEXT NOT NULL,
                pending       TEXT NOT NULL,
                current       TEXT,
                done          TEXT NOT NULL,
                original_head TEXT,
                created_at    INTEGER NOT NULL
            )",
        ),
    ] {
        if table_columns(conn, table)?.is_empty() {
            conn.execute_batch(ddl)?;
            changed = true;
        }
    }

    // The operations table gained a flag for undone rows, so undo can be walked in both
    // directions. A repository created before it needs the column added.
    if !table_columns(conn, "operations")?.is_empty()
        && !table_columns(conn, "operations")?
            .iter()
            .any(|c| c == "undone")
    {
        conn.execute_batch("ALTER TABLE operations ADD COLUMN undone INTEGER NOT NULL DEFAULT 0")?;
        changed = true;
    }

    // Provenance columns, for a repository created before they existed.
    for col in ["session", "intent"] {
        if !table_columns(conn, "changesets")?.iter().any(|c| c == col) {
            conn.execute_batch(&format!("ALTER TABLE changesets ADD COLUMN {} TEXT", col))?;
            changed = true;
        }
    }

    if !obj_cols.iter().any(|c| c == "compressed") {
        conn.execute_batch("ALTER TABLE objects ADD COLUMN compressed INTEGER NOT NULL DEFAULT 0")?;
        changed = true;
    }

    // The base of a delta chain: the object this one is a delta against. NULL means stored whole.
    // A chain is built when a new file shares a large prefix with an existing object; the newest
    // object stores the delta and points to the base. A chain has no cycles because every base is
    // older than the object that points to it, and the depth is capped by MAX_DELTA_DEPTH.
    let _ = ensure_column(conn, "objects", "base_hash", "TEXT")?;
    // Added after `changesets` shipped, so a repository that predates it needs the column and
    // every existing row backfilled. NULL is the marker for "not yet computed": a reader that
    // sees it falls back to the plain walk rather than trusting a number that is not there.
    // The column is new, or a previous run added it and stopped before filling it. Either way the
    // marker is the same: a committed row with no number. Checking the rows rather than only the
    // column is what makes this idempotent, and it is what repairs a repository whose first
    // migration was interrupted. A parent that is not in the table (a shallow import) counts
    // as generation 0, which is what a shallow root is.
    let _ = ensure_column(conn, "changesets", "generation", "INTEGER")?;
    // Asking whether anything is unfilled, not how many. `COUNT(*) ... WHERE generation IS NULL`
    // has no index to help it and no early exit: SQLite reads every row of `changesets` to produce a
    // number, and `migrate` runs on every command, so the cost of opening the repository grew with
    // the length of its history. `EXPLAIN QUERY PLAN` said `SCAN changesets` at both 100 and 1000
    // commits; measured, `migrate` went from 6 ms to 31 ms over that range on an unchanged
    // repository, and that was the whole slope `bench/long_history.py` was reporting.
    //
    // `SELECT 1 ... LIMIT 1` stops at the first row it finds, so the common answer - nothing to do -
    // is O(1) once the table has a row at all, and the slow answer only happens when there really is
    // history to fill.
    let any_unfilled: bool = conn
        .query_row(
            "SELECT 1 FROM changesets WHERE generation IS NULL LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if any_unfilled {
        backfill_generation(conn)?;
        changed = true;
    }
    for col in [
        "ALTER TABLE objects ADD COLUMN group_id INTEGER",
        "ALTER TABLE objects ADD COLUMN offset INTEGER",
        "ALTER TABLE objects ADD COLUMN length INTEGER",
    ] {
        let name = col.split_whitespace().nth(5).unwrap_or("");
        if !obj_cols.iter().any(|c| c == name) {
            let _ = conn.execute(col, []);
            changed = true;
        }
    }
    let _ = ensure_column(conn, "changesets", "git_commit", "TEXT")?;
    // The timezone a git commit was written in. Without it an export cannot reproduce the commit id,
    // because git hashes the offset as part of the author line: a commit written at +0700 and the
    // same commit written at +0000 have different names. NULL means the offset is unknown, which is
    // every changeset created here rather than imported, and those are written as +0000.
    let _ = ensure_column(conn, "changesets", "tz", "TEXT")?;
    // A signature over the changeset's identity, and the public key it verifies against.
    let _ = ensure_column(conn, "changesets", "signature", "TEXT")?;
    let _ = ensure_column(conn, "changesets", "signer", "TEXT")?;
    if !columns_cached(conn, "changesets")?
        .iter()
        .any(|c| c == "parent2")
    {
        conn.execute_batch("ALTER TABLE changesets ADD COLUMN parent2 TEXT")?;
        invalidate_columns("changesets");
    }
    // A branch is unprotected by default, which is what every repository created before this is.
    let _ = ensure_column(conn, "branches", "protected", "INTEGER NOT NULL DEFAULT 0")?;
    // The mode a file carried when it was written: 100644, 100755 for an executable, 120000 for a
    // symlink. Without this column every file is a `100644` blob, so importing a repository that marks
    // a script executable and exporting it back gives git a script it will not run. The column is null
    // for every row written before it existed, and null means `100644`, which is what those rows are.
    let _ = ensure_column(conn, "changeset_files", "mode", "TEXT")?;
    init_schema(conn)?;
    // Stamped last: a database reaching this line has the full shape, so the next open can skip the
    // body. Writing it before init_schema would leave a database that claims the current shape while
    // missing the tables init_schema creates.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    // Migration ran, so the caller has to re-read the schema it cached. The value was always true
    // here; `changed` tracked columns added along the way and nothing read it.
    let _ = changed;
    Ok(true)
}

pub fn table_columns(conn: &Connection, table: &str) -> ZResult<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// object store.

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Hash a file without holding it in memory.
///
/// `hash_bytes` needs the whole body. A scan has to hash every tracked file whether or not it
/// stores one, so a large binary cost its full size in memory just to be named.
pub fn hash_file(p: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

pub fn hash_content(s: &str) -> String {
    hash_bytes(s.as_bytes())
}

/// Store raw bytes as an object. No compression, no delta.
/// Binary files use this: compression often inflates them and deltas are
/// not meaningful for non-text content.
pub fn put_object_bytes(conn: &Connection, bytes: &[u8]) -> ZResult<String> {
    put_object_bytes_for(conn, bytes, None, None)
}

/// Store raw bytes, told which path they came from.
///
/// A `.gitattributes` rule can say a path is large before it is: a 3 MB binary named today that will
/// be 300 MB next month is stored inline by a threshold and copied by every backup from then on. With
/// the path, the rule decides and the threshold is the fallback.
pub fn put_object_bytes_for(
    conn: &Connection,
    bytes: &[u8],
    path: Option<&str>,
    rules: Option<&crate::support::attributes::Rules>,
) -> ZResult<String> {
    let h = hash_bytes(bytes);
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM objects WHERE hash = ?1", params![h], |r| {
            r.get(0)
        })
        .optional()?;
    if exists.is_some() {
        return Ok(h);
    }
    // A large object goes beside the database and its row keeps only the name and size. An inline
    // blob of tens of megabytes is copied by every operation that touches the page: a backup, a
    // VACUUM, a read of one row. The sidecar column is the whole difference and every reader already
    // goes through get_object_bytes, which asks the sidecar first.
    // A rule wins over the threshold in both directions: `large` sends a small body out, `-large`
    // keeps a big one in.
    let send_out = match (path, rules) {
        (Some(p), Some(r)) => r
            .lookup_large(p)
            .unwrap_or_else(|| crate::store::sidecar::is_large(bytes.len())),
        _ => crate::store::sidecar::is_large(bytes.len()),
    };
    if send_out {
        if let Some(root) = db_root(conn) {
            crate::store::sidecar::write(&root, &h, bytes)?;
            conn.execute(
                "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at, sidecar)
                 VALUES(?1, x'', ?2, 0, ?3, 1)",
                params![h, bytes.len() as i64, now()],
            )?;
            return Ok(h);
        }
        // No root to place a file beside: fall through to the inline row, which is correct and slower.
    }
    conn.execute(
        "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at)
         VALUES(?1, ?2, ?3, 0, ?4)",
        params![h, bytes, bytes.len() as i64, now()],
    )?;
    Ok(h)
}

/// Store a large object from a file on disk, never holding it whole.
///
/// The caller has already hashed it (`hash_file`), so the hash is passed in rather than recomputed:
/// hashing a 256 MB file twice would double the only I/O this path cannot avoid.
///
/// A body at or above the sidecar threshold goes beside the database by block copy. A smaller body
/// is read and stored inline, because a body that small is one the inline path was built for.
pub fn put_object_from_file(
    conn: &Connection,
    p: &std::path::Path,
    hash: &str,
    size: u64,
) -> ZResult<String> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM objects WHERE hash = ?1",
            params![hash],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(hash.to_string());
    }
    let root = db_root(conn);
    let send_out = crate::store::sidecar::is_large(size as usize);
    if send_out {
        if let Some(root) = root {
            crate::store::sidecar::write_from_file(&root, hash, p)?;
            conn.execute(
                "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at, sidecar)
                 VALUES(?1, x'', ?2, 0, ?3, 1)",
                params![hash, size as i64, now()],
            )?;
            return Ok(hash.to_string());
        }
        // No root to place a file beside: fall through to reading it, which is correct and slower.
    }
    let bytes = std::fs::read(p)?;
    put_object_bytes_for(conn, &bytes, None, None)?;
    Ok(hash.to_string())
}

fn compress(content: &str) -> ZResult<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(content.as_bytes())?;
    Ok(enc.finish()?)
}

pub fn compress_bytes(data: &[u8]) -> ZResult<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

/// The largest an object may inflate to.
///
/// zlib's ceiling is roughly 1032:1, so a 4 MiB blob can arrive claiming 4 GiB of plaintext, and
/// `read_to_end` will happily allocate it before anything looks at the result. Three read paths did
/// exactly that - `decompress_bytes`, `decompress`, and the pack reader - and each is reachable from a
/// repository or pack file somebody else wrote (#101, #156). 1 GiB is far above any file this store is
/// meant to hold and far below what a bomb needs to be useful.
pub const MAX_INFLATED: usize = 1024 * 1024 * 1024;

/// Inflate with a ceiling, appending at most `cap` bytes.
///
/// `read_to_end` has no limit by design: it grows until the decoder says it is done, which for a bomb
/// is after the whole expansion. `take` bounds the read, and the one byte past the cap is the
/// difference between "it happened to fit" and "it did not finish", so a stream that ends exactly at
/// the cap is accepted and one byte more is refused.
pub fn inflate_capped<R: std::io::Read>(src: R, cap: usize) -> ZResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut limited = src.take(cap as u64 + 1);
    limited
        .read_to_end(&mut out)
        .map_err(|e| bad(format!("a compressed object does not decompress: {}", e)))?;
    if out.len() > cap {
        return Err(bad(format!(
            "a compressed object inflates past {} bytes and was refused; it is either corrupt or a \
             decompression bomb",
            cap
        )));
    }
    Ok(out)
}

pub fn decompress_bytes(blob: &[u8]) -> ZResult<Vec<u8>> {
    inflate_capped(ZlibDecoder::new(blob), MAX_INFLATED)
}

fn decompress(blob: &[u8]) -> ZResult<String> {
    // Through the capped inflater for the same reason as `decompress_bytes`: the row came from
    // somewhere, and "it is only a text object" is not a bound (#101).
    let bytes = inflate_capped(ZlibDecoder::new(blob), MAX_INFLATED)?;
    String::from_utf8(bytes)
        .map_err(|_| bad("a compressed object does not hold valid UTF-8".to_string()))
}

/// Insert content into the object store. Idempotent: identical content maps to
/// one row.
///
/// Compression is applied only when it actually saves bytes. A zlib stream
/// carries a fixed header and trailer, so for a short file compressing makes the
/// row bigger than the text itself; small objects are stored raw instead.
pub fn put_object(conn: &Connection, content: &str) -> ZResult<String> {
    put_object_against(conn, content, None)
}

/// Store text with the path it came from, so a `large` rule applies to it too.
///
/// `put_object_against` compresses and may delta, and never considers the sidecar. That is right for
/// the ordinary case and wrong for a generated file that is 20 MB of UTF-8: the rule says the path is
/// big, and a rule that only moves binaries is one nobody means.
pub fn put_object_for(
    conn: &Connection,
    content: &str,
    path: Option<&str>,
    rules: Option<&crate::support::attributes::Rules>,
    base: Option<&str>,
) -> ZResult<String> {
    let send_out = match (path, rules) {
        (Some(p), Some(r)) => r
            .lookup_large(p)
            .unwrap_or_else(|| crate::store::sidecar::is_large(content.len())),
        _ => crate::store::sidecar::is_large(content.len()),
    };
    if !send_out {
        return put_object_against(conn, content, base);
    }
    // A delta against another object is meaningless for something that is leaving the database: the
    // reader of a sidecar asks for a whole file, and a sidecar holding a delta would need the base.
    put_object_bytes_for(conn, content.as_bytes(), path, rules)
}

/// Store `content`, optionally as a delta against `base`.
///
/// A delta only wins when it is smaller than the compressed whole object, which
/// is measured rather than assumed: a two-line change to a large file produces a
/// tiny delta, while an unrelated rewrite produces a delta bigger than the file
/// and is discarded.
/// How many delta links sit between `hash` and a whole object.
///
/// Nothing capped this, so a file edited 120 times stored a 120 link chain and
/// every read of the newest version paid 120 decompressions. Reading the newest
/// version took 131 ms on a 200 line file with 120 versions. The cap below keeps
/// a read at a constant cost.
fn delta_depth(conn: &Connection, hash: &str) -> ZResult<usize> {
    // One query for the whole chain. This used to be a query per link, and it is called for every
    // candidate a delta base search considers, so a chain sixteen deep cost sixteen queries before
    // anything was decided. `bench/long_history.py` appends to one file, which is exactly the shape
    // that builds a deep chain, and the commit got 5.91x slower over a tenfold history once the base
    // search existed: the gate caught it on CI while the local machine was too noisy to show it.
    //
    // The cap is in the query, so a corrupt store with a cycle still terminates.
    let depth: i64 = conn.query_row(
        "WITH RECURSIVE chain(hash, base, n) AS (
                 SELECT hash, base, 0 FROM deltas WHERE hash = ?1
                 UNION ALL
                 SELECT d.hash, d.base, chain.n + 1
                 FROM deltas d JOIN chain ON d.hash = chain.base
                 WHERE chain.n < ?2
             )
             SELECT COALESCE(MAX(n) + 1, 0) FROM chain",
        params![hash, MAX_DELTA_DEPTH as i64],
        |r| r.get(0),
    )?;
    Ok(depth as usize)
}

/// Delta chains are capped so that reading the newest version costs the same
/// whether a file has ten edits or ten thousand. git has the same problem and
/// the same shape of answer (delta chain limits plus periodic repacking).
const MAX_DELTA_DEPTH: usize = 16;

/// How much of two bodies is compared when deciding whether one is a useful base for the other.
///
/// The probe used to walk the whole length of the smaller body. `bench/long_history.py` appends to
/// one file and commits, so both bodies grow with the history and the comparison grew with them:
/// the cost of a commit was O(history) even after the base search itself was fixed. A fixed window
/// keeps the decision cheap and still finds the base for the case deltas exist for - a file edited
/// in place, which shares its beginning. A body whose only shared region lies past this window
/// would produce a payload encoding everything before it, so such a base is not worth having.
const DELTA_PROBE_WINDOW: usize = 4096;

/// How many candidates a base search will inflate before it gives up and stores the body whole.
///
/// The loop used to stop after four *successful* probes, but a probe that fails the prefix test
/// costs the same inflation and did not count, so a file with a long history could inflate every
/// candidate in the band on every commit. Six is enough to find the body this one is an edit of -
/// the immediately preceding versions - and small enough that the work stays bounded.
const MAX_PROBE_ATTEMPTS: usize = 6;

pub fn put_object_against(conn: &Connection, content: &str, base: Option<&str>) -> ZResult<String> {
    let h = hash_content(content);
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM objects WHERE hash = ?1", params![h], |r| {
            r.get(0)
        })
        .optional()?;
    if exists.is_some() {
        return Ok(h);
    }

    let raw = content.as_bytes().to_vec();
    let packed = compress(content)?;
    let (blob, compressed) = if packed.len() < raw.len() {
        (packed.clone(), 1i64)
    } else {
        (raw, 0i64)
    };
    let whole_size = blob.len();

    // If no base was provided, try to find one that shares a large prefix.
    // The idea is: a new file that starts with the same 200-line prologue as an existing file
    // should be stored as a delta against that file. This is the shape `bench/delta_share.py`
    // measured at 9.43x saving, but it was never implemented because choosing a base requires
    // an index over object sizes and a similarity guess. Here we use a simple heuristic:
    // scan objects of similar size and pick the one with the longest common prefix.
    let base_hash = match base {
        Some(b) => Some(b.to_string()),
        None => find_delta_base(conn, content)?,
    };
    // Track if this is a cross-file delta (auto-found base) vs same-file delta (provided base).
    let is_cross_file = base.is_none() && base_hash.is_some();

    // Try a delta only when a base is offered and it is a different object.
    if let Some(base_hash) = base_hash {
        if base_hash != h {
            // A base that is already at the cap becomes a whole object instead,
            // which restarts the chain. Without this the chain grows with the
            // number of edits and every read gets slower.
            let at_cap = delta_depth(conn, &base_hash)? >= MAX_DELTA_DEPTH;
            let base_text = if at_cap {
                None
            } else {
                // A base that cannot be read cannot be a delta base. Falling back to a whole
                // object is the only answer that keeps the write: refusing here would turn a
                // missing base into a failed commit.
                get_object(conn, &base_hash).ok()
            };
            if let Some(base_text) = base_text {
                if let Some(ops) = crate::store::delta::encode(&base_text, content) {
                    let payload = crate::store::delta::to_bytes(&ops);
                    let packed_delta = compress_bytes(&payload)?;
                    // A delta is only worth it when it beats the compressed
                    // whole object by a real margin, not by a few bytes.
                    // For cross-file deltas (shared prologue), use a smaller margin because the
                    // savings are amortized across multiple files. Compare against raw size for
                    // cross-file, since the delta chain saves space even if individual delta
                    // doesn't beat zlib on its own.
                    let delta_margin = if is_cross_file { 0 } else { 32 };
                    // For cross-file, compare against raw content size to reflect amortized savings.
                    let threshold = if is_cross_file {
                        content.len()
                    } else {
                        whole_size
                    };
                    if packed_delta.len() + delta_margin < threshold {
                        // The row and its delta are one write (#116). Two statements outside a
                        // transaction leave a window where `objects` holds a hash with an empty body
                        // and no `deltas` row to reconstruct it: every read of that hash then fails
                        // as "corrupt", and the object it was supposed to be is unreadable forever.
                        //
                        // A savepoint rather than a transaction: `put_object_against` is called from
                        // inside a commit's transaction, and `BEGIN` there is an error rather than a
                        // nested one. The savepoint nests either way, which is the same reasoning as
                        // `set_branch_head` (#96).
                        conn.execute_batch("SAVEPOINT put_delta")?;
                        let wrote = (|| -> ZResult<()> {
                            conn.execute(
                                "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at, base_hash)
                                 VALUES(?1, X'', ?2, 0, ?3, ?4)",
                                params![h, content.len() as i64, now(), base_hash],
                            )?;
                            conn.execute(
                                "INSERT OR REPLACE INTO deltas(hash, base, payload, created_at)
                                 VALUES(?1, ?2, ?3, ?4)",
                                params![h, base_hash, packed_delta, now()],
                            )?;
                            Ok(())
                        })();
                        match wrote {
                            Ok(()) => {
                                conn.execute_batch("RELEASE put_delta")?;
                                return Ok(h);
                            }
                            Err(e) => {
                                // Neither row stays, so the hash is simply absent and the caller's
                                // retry is a clean write rather than a read of half an object.
                                let _ = conn.execute_batch("ROLLBACK TO put_delta");
                                let _ = conn.execute_batch("RELEASE put_delta");
                                return Err(e);
                            }
                        }
                    }
                }
            }
        }
    }

    conn.execute(
        "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at, base_hash)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            h,
            blob,
            content.len() as i64,
            compressed,
            now(),
            None::<String>
        ],
    )?;
    Ok(h)
}

/// Find an object that shares a large prefix with the given content.
///
/// This is the base-selection heuristic for cross-file delta chains. The idea is simple:
/// look at objects of similar size (within 2x) that are not already at max delta depth,
/// and pick the one with the longest common prefix with the new content. A prefix is
/// counted in bytes, and we require at least 50% of the smaller body to be shared.
///
/// The scan is bounded: we only look at the 50 largest objects, because a delta base
/// is only useful when the shared prefix is large, and a large shared prefix implies
/// the base is also large.
fn find_delta_base(conn: &Connection, content: &str) -> ZResult<Option<String>> {
    let target_len = content.len();
    if target_len < 256 {
        return Ok(None); // too small to benefit from a delta
    }

    // Candidates: objects of similar size, not at max delta depth, not the same content.
    //
    // The ORDER BY used to be `base_hash IS NULL DESC, size DESC` to prefer a depth-0 base. That
    // expression is computed per row, so SQLite could not use an index for it and fell back to
    // reading the range and sorting it, which meant the query touched the whole store on every
    // commit. Ordering by size alone lets `idx_objects_size` serve both the range and the order;
    // the depth-0 preference is applied below, where the rows have been read anyway.
    // The window has to shrink as the body grows, not widen with it. `long_history.py` appends one
    // line per commit, so a file's size tracks the history: a 2x window around 8 kB covered 495
    // objects at 1000 commits and 50 at 100, which is exactly the 10x the gate measured. The number
    // of candidates a search has to weigh was rising with the repository while the work per
    // candidate stayed the same.
    //
    // A body worth basing on is one this commit is an edit of, and that object was written by a
    // recent commit, so recency is the useful filter. The size band still has to hold - a delta
    // against something twice the size pays for the copy - but a narrow band plus a recent-first
    // order keeps the candidate set bounded no matter how long the file has been growing.
    let slack = (target_len / 8).max(64);
    // A body that has been growing for a thousand commits has a thousand siblings within any
    // useful band, because each one was one line shorter than the next. Comparing all of them costs
    // the same work per candidate no matter how the band is chosen, so the bound that matters is the
    // count: the candidate this commit is an edit of was written by one of the last few commits that
    // touched this size, and anything older shares proportionally less of its beginning.
    let size_min = target_len.saturating_sub(slack);
    let size_max = target_len + slack;
    let mut stmt = conn.prepare(
        "SELECT hash, size, base_hash FROM objects
         WHERE size BETWEEN ?1 AND ?2
         ORDER BY created_at DESC
         LIMIT 24",
    )?;
    let candidates = stmt.query_map(params![size_min as i64, size_max as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;

    let target = content.as_bytes();

    // Read the candidates into memory once. Each one used to be resolved with `get_object`, which
    // decompresses a blob and follows a delta chain, and `delta_depth` was a query per candidate.
    // That is up to fifty decompressions and fifty queries on every commit, and the store only grows,
    // so the cost of a commit grew with it: `bench/long_history.py` measured 3.14x over a 10x history.
    // A probe reads the first bytes of each object without decompressing the whole body.
    let mut best: Option<(String, usize)> = None;
    let mut considered = 0usize;
    // `considered` counts bases that passed the probe, and the loop breaks on it. That bounded how
    // many deltas were weighed but not how much work was done: a candidate that failed the prefix
    // test never incremented it, so the loop could inflate every candidate in the band - up to
    // twenty-four bodies, each of which grows with the file - and still count zero. The counter that
    // stops the loop has to count attempts, not successes.
    let mut attempts = 0usize;

    for c in candidates.flatten() {
        // Cheap rejection first. The depth check below is a recursive query, and running it for every
        // candidate before the prefix probe meant up to fifty chain walks per commit; each walk is
        // short, but the number of candidates that have a base at all grows with the history, so the
        // cost of a commit grew with it. The prefix probe is a bounded read of the first bytes and
        // rejects almost everything, which is why it goes first even though it looks like detail.
        attempts += 1;
        if attempts > MAX_PROBE_ATTEMPTS {
            break;
        }
        let Some(head) = object_prefix(conn, &c.0, 64)? else {
            continue;
        };
        if target.len() < 64 || head.len() < 64 || target[..64] != head[..64] {
            continue;
        }
        // A candidate that got this far is worth the chain walk. A deep chain would make every later
        // read of the object built on it walk too, so it is not a base: a whole object is best, and
        // anything past a quarter of the cap costs more to read than the delta saves.
        if c.2.is_some() && delta_depth(conn, &c.0)? > MAX_DELTA_DEPTH / 4 {
            continue;
        }
        considered += 1;
        // Only now is the body worth reading.
        let Some(cand) = get_object(conn, &c.0).ok() else {
            continue;
        };
        let cand_bytes = cand.as_bytes();
        let limit = target_len.min(cand_bytes.len());
        // Compare a bounded window, not the whole file. `bench/long_history.py` appends to one file
        // and commits, so the file grows with the history: an unbounded prefix walk meant the cost
        // of a commit grew with the repository, which is the shape that gate measures (5.98x over a
        // 10x history on CI). The window is what makes a delta worth writing - a shared prologue -
        // and its cost has to be constant for the gate to mean anything.
        //
        // A file edited in place still shares its whole beginning, so the cheaper probe finds the
        // same base. A file whose only shared region lies past this window is a worse delta anyway:
        // the payload would have to encode everything before it.
        let window = limit.min(DELTA_PROBE_WINDOW);
        let shared = lcs_prefix_len(target, cand_bytes, window);
        if shared < window / 2 {
            continue;
        }
        // Prefer a depth-0 base, which keeps the chain shallow: it scores a tenth of the
        // shared length higher. Read from the row already in hand rather than a second query.
        let score = shared + if c.2.is_none() { limit / 10 } else { 0 };
        let best_score = best
            .as_ref()
            .map(|(h, s)| {
                *s + if base_is_depth_zero(conn, h) {
                    limit / 10
                } else {
                    0
                }
            })
            .unwrap_or(0);
        if score > best_score {
            best = Some((c.0, shared));
        }
        // One candidate that shares a large prefix is enough; scanning fifty to find a marginally
        // better one costs more than the delta it buys.
        if considered >= 4 {
            break;
        }
    }

    Ok(best.map(|(h, _)| h))
}

/// The first `n` bytes of an object's body, or None when it cannot be read.
///
/// An uncompressed row is cut without touching the rest, which is the common case for text this
/// store keeps raw. A compressed row still has to be inflated to look at its prefix, because zlib
/// gives no way to ask for the first bytes of the output without running the decoder. So this is
/// cheaper than `get_object` for the raw case and the same for the compressed one; what it avoids
/// is resolving a delta chain, which `get_object` does and a prefix comparison does not need.
fn object_prefix(conn: &Connection, hash: &str, n: usize) -> ZResult<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>, i64, Option<i64>)> = conn
        .query_row(
            "SELECT content, compressed, length FROM objects WHERE hash = ?1",
            params![hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        // A body stored whole and uncompressed is its own prefix.
        Some((blob, 0, _)) => Ok(Some(blob.into_iter().take(n).collect())),
        // A body in a sidecar or a shared stream is not in `content`, so there is no prefix to cut
        // cheaply; refusing it as a candidate is correct and costs nothing.
        Some((blob, _, Some(_))) => {
            if blob.is_empty() {
                return Ok(None);
            }
            match decompress_bytes(&blob) {
                Ok(body) => Ok(Some(body.into_iter().take(n).collect())),
                Err(_) => Ok(None),
            }
        }
        // Compressed: inflate, then cut. This is the expensive path, and it is the one that made the
        // cost of a commit grow with the file: `long_history.py` appends a line per commit, so every
        // candidate in the size band is a body that grows with the history, and inflating all of
        // them to read 64 bytes each cost more than the delta it was deciding to write.
        //
        // The objects that matter for a base are the ones this commit is an edit of, and an edit of
        // a growing file produces a body one line longer than the last. Those were written moments
        // ago and are still in the page cache; older siblings are both colder and share less. The
        // caller bounds the count for that reason, so this stays a bounded number of inflations.
        Some((blob, _, None)) => match decompress_bytes(&blob) {
            Ok(body) => Ok(Some(body.into_iter().take(n).collect())),
            Err(_) => Ok(None),
        },
    }
}

/// Whether this object is stored whole, which is what makes it a good delta base.
fn base_is_depth_zero(conn: &Connection, hash: &str) -> bool {
    conn.query_row(
        "SELECT base_hash IS NULL FROM objects WHERE hash = ?1",
        params![hash],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// Length of the longest common prefix between two byte slices, up to `limit` bytes.
///
/// This is a simple byte-wise comparison, not an LCS, because the shared prefix in
/// source files is almost always a verbatim block (imports, licence, comments).
fn lcs_prefix_len(a: &[u8], b: &[u8], limit: usize) -> usize {
    let limit = limit.min(a.len()).min(b.len());
    let mut i = 0;
    while i < limit && a[i] == b[i] {
        i += 1;
    }
    i
}

pub fn delta_stats(conn: &Connection) -> ZResult<(i64, i64)> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM deltas", [], |r| r.get(0))
        .unwrap_or(0);
    if count == 0 {
        return Ok((0, 0));
    }
    let mut stmt = conn.prepare(
        "SELECT d.hash, LENGTH(d.payload), o.size FROM deltas d
         JOIN objects o ON o.hash = d.hash",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut saved = 0i64;
    for r in rows {
        let (_h, payload, raw) = r?;
        // Compare against the raw size, which is the optimistic alternative: a
        // real whole object would also be compressed, so the true saving is
        // smaller than this. Reported as an upper bound.
        saved += (raw - payload).max(0);
    }
    Ok((count, saved))
}

/// The store's root directory, for a caller that has to reach the sidecar directly.
///
/// `db_root` stays private because most code should not need a path at all, but an importer that
/// resolves a pointer needs the directory the objects live in.
pub fn root_of(conn: &Connection) -> Option<std::path::PathBuf> {
    db_root(conn)
}

/// Whether this object's bytes live in a file beside the database rather than inside a row.
///
/// A caller that writes the object somewhere else needs to know the difference, because the two
/// shapes are not interchangeable downstream: a git export writes the bytes into a blob and a
/// large one has to leave as a pointer instead. Reading the flag is cheaper and more honest than
/// guessing from the size, because a `.gitattributes` rule can send a small file out and keep a
/// large one in.
pub fn object_is_sidecar(conn: &Connection, hash: &str) -> bool {
    conn.query_row(
        "SELECT sidecar FROM objects WHERE hash = ?1",
        params![hash],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(false)
}

pub fn get_object(conn: &Connection, hash: &str) -> ZResult<String> {
    get_object_at_depth(conn, hash, 0)
}

/// `get_object`, with the number of times this read has already re-entered.
///
/// The recursive CTE below caps each *query* at `MAX_DELTA_DEPTH` links, but a chain longer than the
/// cap is finished by calling `get_object` on the far end - and that call had no depth counter, so a
/// cycle in `deltas` (A -> B -> A, which a corrupt or hand-written database can hold) recursed until
/// the stack overflowed and took the process with it (#94). A read of one object must never be able
/// to end the process. The cap is the same limit the CTE uses, so a chain the store is willing to
/// build is a chain it can read.
fn get_object_at_depth(conn: &Connection, hash: &str, depth: usize) -> ZResult<String> {
    if depth * MAX_DELTA_DEPTH > MAX_DELTA_DEPTH * MAX_DELTA_DEPTH {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Corrupt,
            format!(
                "the delta chain under {} does not terminate after {} links; the store holds a cycle",
                hash,
                MAX_DELTA_DEPTH * MAX_DELTA_DEPTH
            ),
        )));
    }
    // A stored object is either whole, or a delta against another object, and the base may itself
    // be a delta. One query fetches the whole chain below.
    // Read the whole chain in one query, then apply it from the base upwards. This used to be two
    // queries per link plus a hash check per link, so reading a file whose last edit was sixteen
    // deltas deep cost thirty-two round trips to reconstruct 311 bytes. `bench/long_history.py`
    // appends to one file, so that is the shape a real repository has when a file is edited in
    // place: the chain grows one link per commit and never shortens.
    //
    // The hash is still checked, once per link, because it is the contract: a rebuild that does not
    // match means the store holds bytes it cannot vouch for, and that has to surface as corruption
    // rather than as wrong content. What changed is where the bytes come from, not what is checked.
    let chain: Vec<(String, Vec<u8>)> = {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE walk(hash, base, payload, n) AS (
                 SELECT hash, base, payload, 0 FROM deltas WHERE hash = ?1
                 UNION ALL
                 SELECT d.hash, d.base, d.payload, walk.n + 1
                 FROM deltas d JOIN walk ON d.hash = walk.base
                 WHERE walk.n < ?2
             )
             SELECT hash, payload FROM walk ORDER BY n DESC",
        )?;
        let rows = stmt.query_map(params![hash, MAX_DELTA_DEPTH as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let Some((deepest, _)) = chain.first() else {
        return read_whole(conn, hash);
    };
    // The far end of the chain is a whole object. If it is not, the chain is longer than the cap
    // and the payload at the far end is a delta against something this read did not fetch; reading
    // the whole object there keeps the answer correct and is what the recursion did before.
    let base_hash: Option<String> = conn
        .query_row(
            "SELECT base FROM deltas WHERE hash = ?1",
            params![deepest],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let mut body = match base_hash {
        Some(root) => get_object_at_depth(conn, &root, depth + 1)?,
        None => read_whole(conn, deepest)?,
    };

    // Apply from the oldest link forwards: each payload is a delta against the body built so far.
    for (link, packed) in &chain {
        let raw = decompress_bytes(packed)?;
        let ops = crate::store::delta::from_bytes(&raw)
            .ok_or_else(|| bad(format!("delta for {} is malformed", link)))?;
        body = crate::store::delta::apply(&body, &ops)
            .ok_or_else(|| bad(format!("delta for {} does not apply to its base", link)))?;
        // Checked per link, not once at the end: a walk that only checks the last hash can say the
        // store is corrupt but not which link broke, and repairing a chain means knowing where it
        // broke. The cost is one hash per link, and a link is only walked when the object is read.
        if hash_content(&body) != *link {
            return Err(Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Corrupt,
                format!("rebuilt object {} does not match its hash", link),
            )));
        }
    }

    let rebuilt = body;
    if hash_content(&rebuilt) != hash {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Corrupt,
            format!("rebuilt object {} does not match its hash", hash),
        )));
    }
    Ok(rebuilt)
}

/// Read an object as raw bytes.
///
/// Binary files are stored uncompressed and are not valid UTF-8, so the text reader would
/// refuse them with a message about encoding. A caller that is about to write bytes to a file
/// wants them as bytes.
pub fn get_object_bytes(conn: &Connection, hash: &str) -> ZResult<Vec<u8>> {
    // A delta is always line deltas over text; a binary object never has one.
    let delta: Option<i64> = conn
        .query_row("SELECT 1 FROM deltas WHERE hash = ?1", params![hash], |r| {
            r.get(0)
        })
        .optional()?;
    if delta.is_some() {
        return Ok(get_object(conn, hash)?.into_bytes());
    }
    // A grouped object keeps its body in a shared stream. Asking for the group first is one column
    // read, and it keeps every caller on one path: nothing outside this function needs to know
    // whether an object is packed.
    if let Ok(crate::store::pack::Placement::Packed {
        group,
        offset,
        length,
    }) = crate::store::pack::placement(conn, hash)
    {
        if let Some(body) = crate::store::pack::read(conn, group, offset, length)? {
            return Ok(body);
        }
    }
    // A sidecar object has no body in its row. Asked before the row, because the row is present with
    // an empty body and reading it would answer "empty object" for a large one.
    if let Some(root) = db_root(conn) {
        if let Some(bytes) = crate::store::sidecar::read(&root, hash) {
            return Ok(bytes);
        }
    }
    let row: Option<(Vec<u8>, i64, i64)> = conn
        .query_row(
            "SELECT content, compressed, sidecar FROM objects WHERE hash = ?1",
            params![hash],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    // The row says its body is in a sidecar file and the file is not there. Falling through used to
    // return the row's empty body (#95), so a deleted, moved or never-written sidecar turned a large
    // object into zero bytes with no error, and every reader - `show`, checkout, status hashing -
    // then worked on wrong content instead of reporting that the object is gone. A grouped object
    // whose group row is missing already fails closed, because `decompress_bytes(x'')` errors; this
    // is the same case and now answers the same way.
    // The row's own `sidecar` column is the only evidence that matters. An absent file is not
    // evidence on its own: every inline object on a file-backed store has no sidecar file, and
    // treating "no file" as "the file is missing" refused every object in the repository. That was
    // the first version of this fix, and nine tests across pack, gitserver and http said so.
    if row.as_ref().is_some_and(|(_, _, s)| *s != 0) {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Corrupt,
            format!(
                "object {} is marked as living in a sidecar file, and that file is missing; the store is damaged",
                hash
            ),
        )));
    }
    let (blob, compressed, _) = row.ok_or_else(|| {
        bad(format!(
            "object {} not found (store may be corrupted)",
            hash
        ))
    })?;
    if compressed == 1 {
        decompress_bytes(&blob)
    } else {
        Ok(blob)
    }
}

fn read_whole(conn: &Connection, hash: &str) -> ZResult<String> {
    // A grouped object has no body in its own row, so the group is asked first. Both readers go
    // through here, and only one of them knew about groups at first: `show` failed with "incomplete
    // deflate stream" because it read the empty `content` column and tried to decompress nothing.
    if let Ok(crate::store::pack::Placement::Packed {
        group,
        offset,
        length,
    }) = crate::store::pack::placement(conn, hash)
    {
        if let Some(body) = crate::store::pack::read(conn, group, offset, length)? {
            return String::from_utf8(body)
                .map_err(|e| bad(format!("object {} is not valid UTF-8: {}", hash, e)));
        }
    }
    // A sidecar object keeps its bytes outside. Both readers branch here, because only one of them
    // knowing about a storage shape is how `show` failed on grouped objects.
    if let Some(root) = db_root(conn) {
        if let Some(bytes) = crate::store::sidecar::read(&root, hash) {
            return String::from_utf8(bytes)
                .map_err(|e| bad(format!("object {} is not valid UTF-8: {}", hash, e)));
        }
    }
    let row: Option<(Vec<u8>, i64, i64)> = conn
        .query_row(
            "SELECT content, compressed, sidecar FROM objects WHERE hash = ?1",
            params![hash],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    // The sidecar flag is read here too (#277). `get_object_bytes` was fixed under #95 to fail
    // closed when the row says its body lives in a sidecar file and that file is gone; this reader
    // kept selecting two columns, never saw the flag, and returned the row's empty body as `Ok("")`.
    // Two readers of one object then answered differently: bytes said Corrupt, text said the file is
    // empty. Reachable from `get_object`, so `find_delta_base`, `show` and `diff` could build on an
    // empty string for an object the store had already lost.
    if row.as_ref().is_some_and(|(_, _, s)| *s != 0) {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Corrupt,
            format!(
                "object {} is marked as living in a sidecar file, and that file is missing; the store is damaged",
                hash
            ),
        )));
    }
    let (blob, compressed, _) = row.ok_or_else(|| {
        bad(format!(
            "object {} not found (store may be corrupted)",
            hash
        ))
    })?;
    if compressed == 1 {
        decompress(&blob)
    } else {
        String::from_utf8(blob)
            .map_err(|e| bad(format!("object {} is not valid UTF-8: {}", hash, e)))
    }
}

/// (raw bytes of all content, bytes actually stored) for measuring compression.
pub fn object_store_bytes(conn: &Connection) -> ZResult<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(size),0), COALESCE(SUM(LENGTH(content)),0) FROM objects",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

// repo state.

/// Which digest this repository names its objects with.
///
/// A gesta repository keeps no `.git/config` - the whole store is a database and a directory of
/// objects - so `Repo::open` cannot answer this and the served protocol would always claim SHA-1. The
/// answer belongs in the store beside the branch name, and defaults to `Sha1` so every repository
/// written before this existed keeps its meaning.
pub fn hash_kind(conn: &Connection) -> ZResult<gitobj::HashKind> {
    match get_state(conn, "object_format")?.as_deref() {
        Some("sha256") => Ok(gitobj::HashKind::Sha256),
        _ => Ok(gitobj::HashKind::Sha1),
    }
}

/// Record the format this repository's object names use.
pub fn set_hash_kind(conn: &Connection, kind: gitobj::HashKind) -> ZResult<()> {
    set_state(conn, "object_format", kind.as_str())
}

pub fn get_state(conn: &Connection, key: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM repo_state WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

pub fn set_state(conn: &Connection, key: &str, value: &str) -> ZResult<()> {
    conn.execute(
        "INSERT INTO repo_state(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn current_branch(conn: &Connection) -> ZResult<String> {
    // The branch belongs to the working tree, not to the store. Two trees can share one store - a
    // linked worktree is exactly that - and each holds a different branch, so a single row in the
    // shared state can only ever describe one of them. Whichever tree this process is working in
    // records its branch before anything reads it, and that answer wins.
    //
    // Falling back to the store keeps every existing repository reading as it did: a repository with
    // one tree never writes the per-tree value, so this branch of the code is the only one it takes.
    if let Some(name) = WORKTREE_BRANCH.get() {
        return Ok(name.clone());
    }
    Ok(get_state(conn, "current_branch")?.unwrap_or_else(|| "main".to_string()))
}

/// The branch the working tree of this process holds, when it differs from the store's.
static WORKTREE_BRANCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Point the rest of this process at the branch a particular working tree holds.
///
/// Called once, at startup, after the repository root is known. A second call is ignored rather than
/// panicking: a process has one working tree, and the first answer is the true one.
pub fn set_worktree_branch(branch: &str) {
    let _ = WORKTREE_BRANCH.set(branch.to_string());
}

// graph.

/// Parents of a changeset. Public so a history walk outside this module can
/// follow the chain without duplicating the query.
pub fn parents_of_public(conn: &Connection, id: &str) -> ZResult<(Option<String>, Option<String>)> {
    parents_of(conn, id)
}

fn parents_of(conn: &Connection, id: &str) -> ZResult<(Option<String>, Option<String>)> {
    Ok(conn
        .query_row(
            "SELECT parent, parent2 FROM changesets WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?
        .unwrap_or((None, None)))
}

/// Every changeset id reachable from `branch`'s head, following both parents so
/// a merge keeps its full history.
pub fn reachable_from(conn: &Connection, branch: &str) -> ZResult<Vec<String>> {
    match branch_head(conn, branch)? {
        Some(head) => Ok(ancestors(conn, &head)?),
        None => Ok(Vec::new()),
    }
}

/// `id` plus all its ancestors, in discovery order (newest first).
///
/// At the sizes this store was measured at the plain walk is under 10 ms, so this stays a walk and
/// the only thing it saves is the repeated SELECT: the whole parent map is read once instead of one
/// query per visited changeset. Generation numbers would let it prune, and each row now carries one
/// (see `changesets.generation`), but pruning only pays when the walk is long enough to matter and a
/// repository that has not been backfilled has no numbers to prune with. Reading the map first is the
/// part that helps every repository, old or new, so it is the part that shipped.
pub fn ancestors(conn: &Connection, id: &str) -> ZResult<Vec<String>> {
    Ok(walk_ancestors(&parent_map(conn)?, id))
}

/// The walk itself, over a parent map the caller already holds.
///
/// Split out so `merge_bases` can walk several roots without re-reading the table each time; the
/// public `ancestors` is the one-shot form with the read included.
fn walk_ancestors(parents: &ParentMap, id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        out.push(cur.clone());
        if let Some((p1, p2)) = parents.get(&cur) {
            if let Some(p) = p1 {
                stack.push(p.clone());
            }
            if let Some(p) = p2 {
                stack.push(p.clone());
            }
        }
    }
    out
}

/// A changeset's two parents, by id. A merge has both; everything else has one or none.
pub type ParentMap = std::collections::HashMap<String, (Option<String>, Option<String>)>;

/// Every changeset's parents, for a caller that will walk more than once.
///
/// `is_ancestor` is the one-shot answer and builds this map per call. A caller that asks ancestry
/// repeatedly against one history - `landed` does, at every step of its search - has to hold the map
/// itself or pay a full table read per question.
pub fn parent_map_for(conn: &Connection) -> ZResult<ParentMap> {
    parent_map(conn)
}

/// Does walking parents from `desc` reach `anc`?
///
/// The map is the caller's, so a loop over candidates reads the table once rather than once per
/// candidate. This is `is_ancestor` without either the generation shortcut or the per-call read.
pub fn walk_reaches(parents: &ParentMap, desc: &str, anc: &str) -> bool {
    if desc == anc {
        return true;
    }
    walk_ancestors(parents, desc).iter().any(|id| id == anc)
}

/// The first-parent chain from `id` towards the root, newest first.
///
/// The first parent is the line a branch moved along: a merge's second parent is the work that
/// arrived, and following both would answer a different question. The bound exists because a
/// corrupt history can hold a cycle - `fsck` reports one, and a walk that ran forever would report
/// it as a hang instead.
pub fn first_parent_chain(conn: &Connection, id: &str) -> ZResult<Vec<String>> {
    const CAP: usize = 1_000_000;
    let mut out = Vec::new();
    let mut cur = Some(id.to_string());
    let mut seen = std::collections::HashSet::new();
    while let Some(at) = cur {
        // A cycle would repeat; stopping at the repeat is the bounded answer and matches what the
        // walk-based caller did before this existed.
        if !seen.insert(at.clone()) || out.len() >= CAP {
            break;
        }
        let (p1, _p2) = parents_of(conn, &at)?;
        out.push(at);
        cur = p1;
    }
    Ok(out)
}

/// Every changeset's parents, in one query.
///
/// `ancestors` used to ask the database once per visited changeset, and `merge_bases` calls it once
/// for each of its inputs and again for every common ancestor. On a linear history of n, answering
/// "what is the merge base" cost n queries plus n more per candidate. One map read costs one query
/// and answers all of them.
fn parent_map(conn: &Connection) -> ZResult<ParentMap> {
    let mut stmt = conn.prepare("SELECT id, parent, parent2 FROM changesets")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut map = std::collections::HashMap::new();
    for row in rows {
        let (id, p1, p2) = row?;
        map.insert(id, (p1, p2));
    }
    Ok(map)
}

/// Whether `desc` is `anc` or comes after it.
///
/// The cheap direction first: generations are recorded at commit time and a child is always exactly
/// one more than its highest parent, so a descendant with a generation at or below its candidate
/// ancestor's cannot be descended from it. That check settles the common "no" without reading a row,
/// which is the case a caller asks about most - a client asking whether its head is behind, a push
/// checking that the remote moved forward.
///
/// A generation number is recorded, not derived, so a row that predates the column has NULL. NULL
/// breaks the shortcut, not the answer: the walk handles it, and the walk is the correctness path
/// either way. This is why the function is not called `is_ancestor_by_generation` - it is the answer,
/// with a shortcut where the number allows one.
pub fn is_ancestor(conn: &Connection, anc: &str, desc: &str) -> ZResult<bool> {
    if anc == desc {
        return Ok(true);
    }
    // Read both generations and every parent in one round trip each; the walk below needs the map
    // regardless, so the only thing the generations buy is an early exit.
    let gen_of = |id: &str| -> i64 {
        conn.query_row(
            "SELECT COALESCE(generation, 0) FROM changesets WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
    };
    let (g_anc, g_desc) = (gen_of(anc), gen_of(desc));
    // Zero means the column was NULL, which is also what a missing row returns. A missing row cannot
    // be an ancestor, and a NULL generation cannot be trusted, so only a non-zero pair is a shortcut.
    if g_anc > 0 && g_desc > 0 && g_desc <= g_anc {
        return Ok(false);
    }

    let parents = parent_map(conn)?;
    Ok(walk_ancestors(&parents, desc).iter().any(|id| id == anc))
}

/// Best common ancestor of two changesets: the common ancestor that every other
/// common ancestor also reaches. This is what a three-way merge diffs against.
/// Every best common ancestor of `a` and `b`, sorted newest first.
///
/// A merge base is a common ancestor that no other common ancestor reaches. For an ordinary fork there
/// is one, and a three-way merge needs exactly that. For a criss-cross - two branches that each merged
/// the other, then diverged again - there are two or more, and picking one silently discards the
/// other's changes. git reports all of them and a recursive merge builds a virtual base from the set.
///
/// Returning the set is the honest shape. `merge_base` is the single-answer form, for callers that can
/// only use one, and it says which it picked by being a different function.
pub fn merge_bases(conn: &Connection, a: &str, b: &str) -> ZResult<Vec<String>> {
    // The common case is one side already containing the other: a push that fast-forwards, a
    // branch that has not moved. The answer is the ancestor itself, and generations settle it
    // without building a map. Answered here because the map below is the expensive part.
    if a != b {
        let gen = |id: &str| -> i64 {
            conn.query_row(
                "SELECT COALESCE(generation, 0) FROM changesets WHERE id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
        };
        let (ga, gb) = (gen(a), gen(b));
        // A candidate can only be the ancestor of the other if its generation is lower. That rules
        // out half of the two cases before any walk, and the walk settles which.
        if ga > 0 && gb > 0 && ga != gb {
            let (older, newer) = if ga < gb { (a, b) } else { (b, a) };
            if is_ancestor(conn, older, newer)? {
                return Ok(vec![older.to_string()]);
            }
        }
    }

    // One parent map for the whole answer. The N+1 shape this replaces read the map once per
    // `ancestors` call: twice for the inputs, then once per common ancestor, which on a history
    // that forked and re-merged is the largest part of the cost.
    let parents = parent_map(conn)?;
    let anc_a: HashSet<String> = walk_ancestors(&parents, a).into_iter().collect();
    let common: Vec<String> = walk_ancestors(&parents, b)
        .into_iter()
        .filter(|x| anc_a.contains(x))
        .collect();
    if common.is_empty() {
        return Ok(Vec::new());
    }

    // Cheap path: one common ancestor is its own answer, and the reachability map below is the
    // expensive part of this function.
    if common.len() == 1 {
        return Ok(common);
    }

    let cache: std::collections::HashMap<String, HashSet<String>> = common
        .iter()
        .map(|c| (c.clone(), walk_ancestors(&parents, c).into_iter().collect()))
        .collect();

    let mut best: Vec<(String, i64)> = Vec::new();
    for cand in &common {
        let reached_by_another = common
            .iter()
            .any(|other| other != cand && cache[other].contains(cand));
        if reached_by_another {
            continue;
        }
        let ts: i64 = conn
            .query_row(
                "SELECT created_at FROM changesets WHERE id = ?1",
                params![cand],
                |r| r.get(0),
            )
            .unwrap_or(0);
        best.push((cand.clone(), ts));
    }

    // Newest first, then by id so the order does not depend on the ancestor walk's traversal order.
    best.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    Ok(best.into_iter().map(|(id, _)| id).collect())
}

/// The single best common ancestor, or none.
///
/// For a criss-cross this picks the newest and silently drops the rest, which is why `merge_bases`
/// exists. Kept for the callers whose answer is a base to diff against rather than a base to merge
/// from, and they should be audited one at a time rather than changed together.
pub fn merge_base(conn: &Connection, a: &str, b: &str) -> ZResult<Option<String>> {
    Ok(merge_bases(conn, a, b)?.into_iter().next())
}

/// Build the `IN (?, ?, ...)` placeholder list for `n` bound values.
///
/// The only reason any query in this program is assembled with `format!` is that
/// SQLite has no placeholder for "a list". Nothing from a caller is ever
/// interpolated: this returns question marks and nothing else, and every value
/// travels through `params!`. Keeping it in one function means an audit has one
/// place to look instead of a regex over the whole crate.
pub fn in_placeholders(n: usize) -> String {
    if n == 0 {
        // `IN ()` is a syntax error, so an empty list must match nothing.
        return "NULL".to_string();
    }
    vec!["?"; n].join(",")
}

/// Resolve a revision name to a changeset id.
///
/// A person writes `main:file.txt`, an agent sends `{"rev":"main"}`, and both
/// mean "the head of that branch". A changeset id means that changeset. Trying
/// the branch first and the id second covers both, and returning `None` lets the
/// caller produce one clear error instead of two nested ones.
///
/// This existed as a bug first: `show main:x` passed the branch name into a
/// function that only walked changesets, so a file that was plainly in the head
/// tree was reported missing.
/// Resolve a `vN` version label for a path.
///
/// `history` prints `v26  9cbc02e9` for each row, and a reader who copies that label into `show` was
/// told it did not exist. The label is per path, so this answers only when the caller names one.
pub fn resolve_version_label(conn: &Connection, rev: &str) -> ZResult<Option<(String, String)>> {
    let Some(rest) = rev.strip_prefix('v') else {
        return Ok(None);
    };
    let Ok(n) = rest.parse::<i64>() else {
        return Ok(None);
    };
    if n <= 0 {
        return Ok(None);
    }
    // Ambiguous across paths by construction, so it names the paths it could mean rather than
    // choosing one. `show v26:path` resolves it properly through `resolve_for_path`.
    let paths: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT path FROM file_versions WHERE version = ?1 GROUP BY path ORDER BY path LIMIT 6",
        )?;
        let rows = stmt.query_map(params![n], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if paths.is_empty() {
        return Ok(None);
    }
    if paths.len() > 1 {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Usage,
            format!(
                "{} is a per-file version and {} paths have one: {}. Name the file as {}:<path>.",
                rev,
                paths.len(),
                paths.iter().take(4).cloned().collect::<Vec<_>>().join(", "),
                rev
            ),
        )));
    }
    let row: (String, String) = conn.query_row(
        "SELECT changeset_id, hash FROM file_versions WHERE path = ?1 AND version = ?2",
        params![paths[0], n],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Some(row))
}

/// Resolve `vN` for one path, which is the form that is not ambiguous.
pub fn resolve_version_for_path(
    conn: &Connection,
    rev: &str,
    path: &str,
) -> ZResult<Option<String>> {
    let Some(rest) = rev.strip_prefix('v') else {
        return Ok(None);
    };
    let Ok(n) = rest.parse::<i64>() else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT changeset_id FROM file_versions WHERE path = ?1 AND version = ?2",
            params![path, n],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

pub fn resolve_rev(conn: &Connection, rev: &str) -> ZResult<Option<String>> {
    let rev = rev.trim();

    // `HEAD` is what `status` prints and what every reader of a tool's output copies first. Without
    // this branch it was rejected as a branch named HEAD, which exists in no repository.
    if rev.eq_ignore_ascii_case("HEAD") || rev == "@" {
        let branch = current_branch(conn).unwrap_or_else(|_| "main".to_string());
        return branch_head(conn, &branch);
    }

    if let Ok(Some(head)) = branch_head(conn, rev) {
        return Ok(Some(head));
    }
    let exists: Option<String> = conn
        .query_row(
            "SELECT id FROM changesets WHERE id = ?1",
            params![rev],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(exists);
    }

    // A `vN` label, as `history` prints beside each row. The version is per path, so the label only
    // has a meaning when the caller also names a file; without one it is ambiguous and refused by
    // `resolve_for_path` below rather than guessed at here.
    if let Some((id, _)) = resolve_version_label(conn, rev)? {
        return Ok(Some(id));
    }

    // A git commit id, which `git import` records and `log --oneline` may print. The two names are
    // not derivable from one another, so this is a lookup rather than a transformation.
    let by_commit: Option<String> = conn
        .query_row(
            "SELECT id FROM changesets WHERE git_commit = ?1",
            params![rev],
            |r| r.get(0),
        )
        .optional()?;
    if by_commit.is_some() {
        return Ok(by_commit);
    }
    // An abbreviated git commit, as `history` prints it. Same rule as an id prefix: one match is an
    // answer, several is an ambiguity worth naming, none falls through.
    if rev.len() >= 7 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        let hits: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM changesets WHERE git_commit LIKE ?1 ORDER BY created_at DESC LIMIT 8",
            )?;
            let rows = stmt.query_map(params![format!("{}%", rev)], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if hits.len() == 1 {
            return Ok(Some(hits[0].clone()));
        }
        if hits.len() > 1 {
            return Err(Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Usage,
                format!(
                    "{} matches {} commits. Name more of the id.",
                    rev,
                    hits.len()
                ),
            )));
        }
    }

    // An unambiguous prefix of a changeset id. `log --oneline` prints the short form, and a person
    // copying one out of it was told the changeset did not exist. Two matches is a real ambiguity and
    // has to say so rather than pick one.
    // A prefix as `log` prints it carries the `cs` that every id starts with, so the test is on the
    // whole string being a prefix of an id rather than on it being bare hex.
    let looks_like_prefix = rev.len() >= 4;
    if looks_like_prefix {
        let matches: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM changesets WHERE id LIKE ?1 ESCAPE '!' ORDER BY created_at DESC LIMIT 8",
            )?;
            let prefix = rev.replace('!', "!!").replace('%', "!%").replace('_', "!_");
            let rows = stmt.query_map(params![format!("{prefix}%")], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if matches.len() == 1 {
            return Ok(Some(matches[0].clone()));
        }
        if matches.len() > 1 {
            return Err(Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Usage,
                format!(
                    "{} matches {} changesets: {}. Name more of the id.",
                    rev,
                    matches.len(),
                    matches
                        .iter()
                        .take(3)
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )));
        }
    }
    // A tag is also a valid revision, and a tag may point at another tag. Following the chain
    // rather than reading one row is what keeps a re-tag honest; the bound stops a cycle written
    // by hand from hanging the process.
    let mut cursor = rev.to_string();
    for _ in 0..32 {
        let tagged: Option<String> = conn
            .query_row(
                "SELECT changeset_id FROM tags WHERE name = ?1",
                params![cursor],
                |r| r.get(0),
            )
            .optional()?;
        match tagged {
            None => return Ok(None),
            Some(next) => {
                // A tag that names a real changeset ends the walk, and that is the common case.
                let is_changeset: Option<String> = conn
                    .query_row(
                        "SELECT id FROM changesets WHERE id = ?1",
                        params![next],
                        |r| r.get(0),
                    )
                    .optional()?;
                if is_changeset.is_some() {
                    return Ok(is_changeset);
                }
                cursor = next;
            }
        }
    }
    Err(bad(format!(
        "the tag {} points at tags that never reach a changeset",
        rev
    )))
}

/// Content hash for `path` as of `changeset`, walking its ancestry.
///
/// `changeset_files` is the source of truth, not `file_versions`: a deletion
/// writes a row with `new_hash IS NULL` and no version row, so a lookup that
/// only consulted versions would resurrect deleted files. This is the same rule
/// `tree_at` follows.
pub fn hash_at_point(conn: &Connection, path: &str, changeset: &str) -> ZResult<Option<String>> {
    let reachable = ancestors(conn, changeset)?;
    if reachable.is_empty() {
        return Ok(None);
    }
    let placeholders = in_placeholders(reachable.len());
    let sql = format!(
        "SELECT cf.new_hash FROM changeset_files cf
         JOIN changesets cs ON cs.id = cf.changeset_id
         WHERE cf.path = ? AND cf.changeset_id IN ({})
         ORDER BY cs.created_at DESC, cs.rowid DESC LIMIT 1",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(reachable.len() + 1);
    params.push(&path);
    for id in &reachable {
        params.push(id);
    }
    // A row with a NULL new_hash means the path was deleted at that point, and
    // returning None is exactly right for that case.
    let found: Option<Option<String>> = stmt
        .query_row(params.as_slice(), |r| r.get::<_, Option<String>>(0))
        .optional()?;
    Ok(found.flatten())
}

/// Latest committed content hash for `path` on `branch`.
pub fn latest_hash(conn: &Connection, path: &str, branch: &str) -> ZResult<Option<String>> {
    match branch_head(conn, branch)? {
        Some(head) => hash_at_point(conn, path, &head),
        None => Ok(None),
    }
}

/// Branch head, or `None` for an empty branch.
pub fn branch_head(conn: &Connection, branch: &str) -> ZResult<Option<String>> {
    conn.query_row("SELECT head FROM branches WHERE name = ?1", params![branch], |r| {
        r.get::<_, Option<String>>(0)
    })
    .optional()?
    .ok_or_else(|| {
        // Name what exists, so a typo reads as a typo. A message that only says what is
        // missing leaves the reader unsure whether the branch was never made.
        let known: Vec<String> = conn
            .prepare("SELECT name FROM branches ORDER BY name LIMIT 8")
            .and_then(|mut s| {
                let rows = s.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        // The code travels with the message, so a caller can branch on "no such branch" without
        // matching the sentence. The sentence stays as it was: it names what exists and the fix.
        if known.is_empty() {
            Box::new(crate::support::error::Error::not_found(format!(
                "no such branch: {}; there are no branches yet, create one with `gesta branch create <name>`",
                branch
            ))) as Box<dyn std::error::Error>
        } else {
            Box::new(crate::support::error::Error::not_found(format!(
                "no such branch: {}; known branches: {}",
                branch,
                known.join(", ")
            ))) as Box<dyn std::error::Error>
        }
    })
}

// id allocation.

/// Allocate the next changeset sequence number atomically.
///
/// A read-then-write counter races: two concurrent agents both read N, both
/// write N+1 and then hash the same inputs, so one insert hits the primary key.
/// The increment has to happen inside SQLite itself, which is what RETURNING
/// gives us here.
pub fn next_changeset_seq(conn: &Connection) -> ZResult<u64> {
    let v: i64 = conn.query_row(
        "INSERT INTO repo_state(key, value) VALUES('changeset_seq', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         RETURNING CAST(value AS INTEGER)",
        [],
        |r| r.get(0),
    )?;
    Ok(v.max(1) as u64)
}

/// Allocate a changeset id that is not already taken.
///
/// The id mixes a persisted monotonic sequence with message, author and
/// timestamp. Without the sequence, two agents writing the same message in the
/// same second would produce the same id and the second insert would be lost.
pub fn new_changeset_id(conn: &Connection, message: &str, author: &str) -> ZResult<String> {
    let seq = next_changeset_seq(conn)?;
    for attempt in 0..16u32 {
        let seed = format!("{}|{}|{}|{}|{}", message, author, now(), seq, attempt);
        let id = format!("cs{}", &hash_content(&seed)[..12]);
        let taken: Option<String> = conn
            .query_row(
                "SELECT id FROM changesets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_none() {
            return Ok(id);
        }
    }
    Err(bad(
        "could not allocate a unique changeset id after 16 attempts",
    ))
}

/// Point a branch at `changeset` and keep the committed tree in step.
///
/// Every path that moves a head goes through here. `branch_tree` is derived data,
/// so a writer that forgets to update it makes `status` report files that are
/// already committed. One function is the only way to be sure there is no such
/// writer.
pub fn set_branch_head(conn: &Connection, branch: &str, changeset: &str) -> ZResult<()> {
    // The tree is updated from the changeset's own file list, not rebuilt.
    // Rebuilding reads every path in history, so committing one file cost the
    // size of the repository: 24 ms in a 500 file repository, 67 ms in an 8000
    // file one. Applying only what the changeset touched makes the cost follow
    // the size of the change: the same one file commit now measures 28.7 ms at
    // 500 files and 54.3 ms at 8000, which is 1.89x for a repository 16 times
    // larger (bench/scale.py, median of three rounds, spread 5 to 12 percent).
    //
    // The common case is a head that moves forward onto a commit whose parent is
    // the old head. Anything else (a merge, an undo, a remote apply) is not
    // incremental and falls back to a rebuild, which is correct and rare.
    // The head move and the tree update are one write (#96). A crash between them leaves the head on
    // a commit whose files are not in `branch_tree`, so `status` reports committed files as new and
    // `fsck` is clean because it never compares the two. The doc comment above says one function is
    // the only way to be sure there is no writer that forgets the tree; that guarantee needs the two
    // statements to land together or not at all.
    // SAVEPOINT, not BEGIN (#96). Half the callers pass a bare connection and half pass a
    // transaction they already opened (repo/mod.rs does), and a second BEGIN under an open
    // transaction is an error, not a nested transaction. A savepoint nests in both cases.
    conn.execute_batch("SAVEPOINT set_branch_head")?;
    let outcome = set_branch_head_inner(conn, branch, changeset);
    match outcome {
        Ok(()) => {
            conn.execute_batch("RELEASE set_branch_head")?;
            Ok(())
        }
        Err(e) => {
            // Put the branch back exactly as it was, tree included.
            let _ = conn.execute_batch("ROLLBACK TO set_branch_head");
            let _ = conn.execute_batch("RELEASE set_branch_head");
            Err(e)
        }
    }
}

fn set_branch_head_inner(conn: &Connection, branch: &str, changeset: &str) -> ZResult<()> {
    let old_head = branch_head(conn, branch)?;
    let parent = conn
        .query_row(
            "SELECT parent FROM changesets WHERE id = ?1",
            params![changeset],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    conn.execute(
        "UPDATE branches SET head = ?1 WHERE name = ?2",
        params![changeset, branch],
    )?;

    // A commit whose parent is the old head is a forward step and can be applied.
    // The caller may have re-parented the changeset onto the head just before
    // calling this, which is the same thing and still a forward step: what matters
    // is that the parent chain contains no other parent, since a merge has two.
    let parent2 = conn
        .query_row(
            "SELECT parent2 FROM changesets WHERE id = ?1",
            params![changeset],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let parent_matches = match (&old_head, &parent) {
        (Some(old), Some(p)) => old == p,
        // The first commit on an empty branch has no parent and no old head.
        (None, None) => true,
        _ => false,
    };
    let incremental = parent_matches && parent2.is_none();
    if incremental {
        apply_changeset_to_tree(conn, branch, changeset)?;
    } else {
        refresh_branch_tree(conn, branch)?;
    }
    Ok(())
}

/// Apply one changeset's file list to a branch's tree.
///
/// A path the changeset set to a hash replaces the row; a path it set to no hash
/// is a deletion and its row goes. Nothing else in the tree is read, which is what
/// keeps a commit proportional to the change.
fn apply_changeset_to_tree(conn: &Connection, branch: &str, changeset: &str) -> ZResult<()> {
    let files: Vec<(String, Option<String>)> = {
        let mut stmt =
            conn.prepare("SELECT path, new_hash FROM changeset_files WHERE changeset_id = ?1")?;
        let rows = stmt.query_map(params![changeset], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut put =
        conn.prepare("INSERT OR REPLACE INTO branch_tree(branch, path, hash) VALUES(?1, ?2, ?3)")?;
    let mut del = conn.prepare("DELETE FROM branch_tree WHERE branch = ?1 AND path = ?2")?;
    for (path, hash) in files {
        match hash {
            Some(h) => put.execute(params![branch, path, h])?,
            None => del.execute(params![branch, path])?,
        };
    }
    Ok(())
}

/// Move a branch back to having no head, and drop its tree.
pub fn clear_branch_head(conn: &Connection, branch: &str) -> ZResult<()> {
    // The head and its derived tree are cleared together or not at all (#309).
    //
    // `set_branch_head` was given a savepoint for exactly this reason (#96) and this sibling was
    // missed. A crash or an error between the two statements left `head` NULL with the old tree still
    // in place. The next commit on that branch read `old_head = None`, so the new changeset's parent
    // was NULL, `set_branch_head` took the incremental path, and `apply_changeset_to_tree` applied
    // onto the stale tree - resurrecting every file the tree had already lost.
    //
    // SAVEPOINT, not BEGIN, for the same reason as #96: callers pass both bare connections and
    // transactions they already opened, and a nested BEGIN is an error while a savepoint nests.
    conn.execute_batch("SAVEPOINT clear_branch_head")?;
    let outcome = (|| -> ZResult<()> {
        conn.execute(
            "UPDATE branches SET head = NULL WHERE name = ?1",
            params![branch],
        )?;
        conn.execute("DELETE FROM branch_tree WHERE branch = ?1", params![branch])?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            conn.execute_batch("RELEASE clear_branch_head")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO clear_branch_head");
            let _ = conn.execute_batch("RELEASE clear_branch_head");
            Err(e)
        }
    }
}

/// Rebuild `branch_tree` for one branch from its head.
pub fn refresh_branch_tree(conn: &Connection, branch: &str) -> ZResult<()> {
    conn.execute("DELETE FROM branch_tree WHERE branch = ?1", params![branch])?;
    let head = branch_head(conn, branch)?;
    let Some(head) = head else { return Ok(()) };
    let files = tree_at(conn, &head)?;
    let mut stmt =
        conn.prepare("INSERT OR REPLACE INTO branch_tree(branch, path, hash) VALUES(?1, ?2, ?3)")?;
    for (path, hash) in &files {
        stmt.execute(params![branch, path, hash])?;
    }
    Ok(())
}

/// Reachable changesets in breadth-first order: the id itself, then its parents,
/// then theirs, level by level.
///
/// `ancestors` uses a stack, so its order is depth first and the index of an id
/// says nothing about how close it is to the start. Anything that has to decide
/// which of two changesets is newer needs this order instead, because the only
/// other candidate is the clock and commits made in one burst share a timestamp.
pub fn ancestors_bfs(conn: &Connection, id: &str) -> ZResult<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(id.to_string());
    while let Some(cur) = queue.pop_front() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        out.push(cur.clone());
        let (p1, p2) = parents_of(conn, &cur)?;
        if let Some(p) = p1 {
            queue.push_back(p);
        }
        if let Some(p) = p2 {
            queue.push_back(p);
        }
    }
    Ok(out)
}

/// Reachable changesets with every parent before its children: a topological order.
///
/// `ancestors_bfs` answers "what is reachable, nearest first", and reversing it does not give a
/// topological order (#187). For a head H whose parents are A and B, where B's parent is A, BFS
/// returns `[H, A, B, C]` because A is one hop away and B is two; reversed that is `[C, B, A, H]`,
/// and B is written before A exists. Any caller that builds a structure keyed on "my parent is
/// already there" needs this order instead of a reversed BFS, and the difference only shows on the
/// histories where it matters - criss-cross merges, which git produces on its own.
///
/// Implemented as a depth-first post-order over parents: a node is emitted after everything it
/// depends on. Iterative rather than recursive because a long history would otherwise be a stack
/// overflow on the thing this program is for.
///
/// A cycle is an error, not something to walk forever. The first version of this function had no
/// check, and a `parent` chain that looped made it spin: the parent table is written by several code
/// paths, and a loop in it must be reported, not hang an export with no output. `visiting` marks the
/// nodes currently on the stack, which is exactly the set a back edge can point into.
pub fn topological_order(conn: &Connection, id: &str) -> ZResult<Vec<String>> {
    let mut out = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    // (node, parents already pushed?) so a node is revisited after its parents are emitted.
    let mut stack: Vec<(String, bool)> = vec![(id.to_string(), false)];
    while let Some((cur, expanded)) = stack.pop() {
        if expanded {
            visiting.remove(&cur);
            if done.insert(cur.clone()) {
                out.push(cur);
            }
            continue;
        }
        if done.contains(&cur) {
            continue;
        }
        if !visiting.insert(cur.clone()) {
            return Err(bad(format!(
                "the history loops: {} is its own ancestor, so there is no order to write it in",
                cur
            )));
        }
        // Pushed before the parents so they are emitted first: the stack is last-in first-out.
        stack.push((cur.clone(), true));
        let (p1, p2) = parents_of(conn, &cur)?;
        for p in [p2, p1].into_iter().flatten() {
            if !done.contains(&p) {
                stack.push((p, false));
            }
        }
    }
    Ok(out)
}

/// How many objects the store holds. Used by `gc` to report what it did.
#[cfg(test)]
pub fn object_count(conn: &Connection) -> ZResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))?)
}

/// Collect every object reachable from the repository state, and delete the rest.
///
/// An object is reachable when it belongs to a committed or pending tree, a stash, a branch head,
/// or a shallow snapshot, or is a transitive base of a reachable delta. External git packfiles are
/// separate from this store; a `git gc` on an export handles them.
pub fn gc(conn: &Connection) -> ZResult<u64> {
    // Hold the writer lock from root discovery through deletion so references cannot be added
    // between the live-set snapshot and removal of an object's external bytes.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let root = db_root(conn);
    let mut reachable: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Commits and their trees.
    let mut stmt = conn.prepare("SELECT id FROM changesets WHERE status = 'committed'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for row in rows {
        let cs = row?;
        reachable.insert(cs.clone());
        let tree = tree_at(conn, &cs)?;
        for (_, hash) in tree {
            reachable.insert(hash);
        }
    }

    // Pending changesets (not yet committed but stored).
    let mut stmt = conn.prepare("SELECT id FROM changesets WHERE status = 'pending'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for row in rows {
        let cs = row?;
        reachable.insert(cs.clone());
        let tree = tree_at(conn, &cs)?;
        for (_, hash) in tree {
            reachable.insert(hash);
        }
    }

    // Stashed changes. Read the hashes straight from the table rather than through `stash::list`:
    // `gc` is in the storage layer and everything above it is a caller, so asking the repository layer
    // to unpack stashes would make this module depend on the layer that depends on it. The table is
    // the one `stash` writes, and all `gc` needs is what it points at.
    //
    // The table only exists once something has been stashed. The previous call through `stash::list`
    // created the schema first, which is why preparing this unconditionally turned a clean run into
    // "no such table: stash_files".
    if table_exists(conn, "stash_files")? {
        let mut stash_stmt = conn.prepare("SELECT hash FROM stash_files")?;
        let stash_rows = stash_stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
        for row in stash_rows {
            if let Some(h) = row? {
                reachable.insert(h);
            }
        }
    }

    // Branch heads.
    let mut stmt = conn.prepare("SELECT head FROM branches WHERE head IS NOT NULL")?;
    let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
    for row in rows {
        if let Some(h) = row? {
            reachable.insert(h);
        }
    }

    // Tags point at changesets, which are already reachable through the branch walk, so there
    // is nothing extra to collect here. The table has no hash column; reading one is a mistake
    // that only shows up when a repository has a tag.
    let _ = "tags point at changesets, which are collected above";

    // A shallow clone may keep file bodies only in its snapshot, with no local history row.
    let mut snapshot_stmt = conn.prepare("SELECT hash FROM snapshot_objects")?;
    let snapshot_rows = snapshot_stmt.query_map([], |r| r.get::<_, String>(0))?;
    for hash in snapshot_rows {
        reachable.insert(hash?);
    }

    // Only live deltas keep their bases. Read the edges once and walk to a fixed point;
    // inserting before queueing also bounds a malformed cycle.
    let bases: std::collections::HashMap<String, String> = {
        let mut stmt = conn.prepare("SELECT hash, base FROM deltas")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut pending: Vec<String> = reachable.iter().cloned().collect();
    while let Some(hash) = pending.pop() {
        if let Some(base) = bases.get(&hash) {
            if reachable.insert(base.clone()) {
                pending.push(base.clone());
            }
        }
    }

    // Delete every object not in the reachable set.
    let all: Vec<(String, bool)> = {
        let mut stmt = conn.prepare("SELECT hash, sidecar FROM objects")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut deleted = 0u64;
    for (hash, external) in all {
        if !reachable.contains(&hash) {
            if external {
                let root = root.as_deref().ok_or_else(|| {
                    bad("cannot collect a sidecar object without a repository root")
                })?;
                // Remove only unreachable bytes. Keep the row if removal fails so a later GC
                // retries; a missing file is already reclaimed, including after an interrupted GC.
                match std::fs::remove_file(sidecar::path_for(root, &hash)?) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            conn.execute(
                "DELETE FROM objects WHERE hash = ?1",
                rusqlite::params![&hash],
            )?;
            // The `deltas` row goes with it (#276).
            //
            // `get_object_at_depth` treats the presence of a row in `deltas` as decisive: if one
            // exists it applies the payload and hash-checks each link. An orphaned row left behind
            // by gc therefore outlives the object it described, and if the same content is stored
            // again later - whole, because its base was collected too - every read of that hash finds
            // the stale delta first and fails the hash check. The bytes are fine; the row makes them
            // unreadable.
            //
            // Deleting it here is the same transaction as the object, so a crash between the two
            // cannot leave the pair split.
            conn.execute(
                "DELETE FROM deltas WHERE hash = ?1",
                rusqlite::params![&hash],
            )?;
            deleted += 1;
        }
    }

    // Partial groups retain their compressed stream and offsets; only a group with no remaining
    // members can be reclaimed without rewriting live objects.
    conn.execute(
        "DELETE FROM object_groups WHERE NOT EXISTS
         (SELECT 1 FROM objects WHERE objects.group_id = object_groups.id)",
        [],
    )?;
    tx.commit()?;

    // Vacuum to reclaim space and rebuild indexes.
    conn.execute("VACUUM", [])?;

    Ok(deleted)
}

/// Every path a changeset's tree holds, with its hash, sorted by path.
///
/// Lives here rather than beside the other tree helpers because everything it reads is in this
/// module: `ancestors_bfs`, `in_placeholders`, the changeset_files query. It sat in `repo` instead,
/// which made `store` call up into `repo` while `repo` called down into `store`. That cycle is
/// invisible to the compiler, because this is one crate.
/// The integrity questions `fsck` asks about a store, without a working tree.
///
/// `verify` used to promise "the same integrity questions `fsck` runs" and then ask two of them:
/// `PRAGMA integrity_check` and whether the `changesets` table exists (#81). A backup can pass both
/// and still be unusable - a `changeset_files` row pointing at an object that is not in the file
/// restores into a repository that cannot check anything out, and the restore has already moved the
/// live store aside by then.
///
/// This is the part of `fsck` that needs no filesystem: every referenced object exists, every parent
/// exists (a shallow boundary is not damage), every branch head exists, every sidecar object has its
/// file, and every branch's stored tree equals a rebuild from history. The working-tree check stays in
/// `fsck`, because a backup file has no working tree.
pub fn integrity_problems(
    conn: &Connection,
    root: Option<&std::path::Path>,
) -> ZResult<Vec<String>> {
    use rusqlite::OptionalExtension;
    let mut out = Vec::new();

    // Every changeset_file must point at objects that exist.
    let refs: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT changeset_id, path, old_hash, new_hash FROM changeset_files ORDER BY changeset_id, path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (cs, path, old, new) in &refs {
        for h in [old, new].into_iter().flatten() {
            if get_object_bytes(conn, h).is_err() {
                out.push(format!(
                    "MISSING object {} referenced by {} ({})",
                    h, cs, path
                ));
            }
        }
    }

    // Every stored object must hash to the name it is stored under (#335).
    //
    // `objects.hash` was trusted as an identifier and never re-derived, so replacing an object's
    // bytes left the row, the changeset, the signature and every reader untouched: `show`, `verify`,
    // `fsck` and `checkout` all reported success while serving altered content. A name in a
    // content-addressed store is supposed to *be* the hash of the content.
    //
    // Checked here rather than only in `fsck` because `verify-backup` calls this function to decide
    // whether a backup is usable, and a sealed backup carrying altered bytes was the other half of
    // the hole: the store propagated to another machine and verified there again.
    //
    // `get_object_bytes`, not a raw column read. It is the path every reader takes and it resolves
    // deltas, packed groups and compression; reading the column directly would test something other
    // than what `checkout` serves, and would report a delta object as corrupt because its stored body
    // is a set of line operations rather than the file.
    {
        let mut stmt = conn.prepare("SELECT hash FROM objects ORDER BY hash")?;
        let hashes: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for hash in hashes {
            match get_object_bytes(conn, &hash) {
                Ok(bytes) => {
                    let actual = hash_bytes(&bytes);
                    if actual != hash {
                        out.push(format!(
                            "CORRUPT object {} does not match its name (its content hashes to {})",
                            hash,
                            &actual[..actual.len().min(16)]
                        ));
                    }
                }
                Err(e) => out.push(format!("CORRUPT object {} cannot be read: {}", hash, e)),
            }
        }
    }

    // Every parent must exist, except at a shallow boundary.
    let boundaries: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("SELECT changeset_id FROM shallow_roots")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
    };
    let parents: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, parent, parent2 FROM changesets")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, p1, p2) in &parents {
        for p in [p1, p2].into_iter().flatten() {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM changesets WHERE id = ?1",
                    rusqlite::params![p],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() && !boundaries.contains(id) {
                out.push(format!("MISSING parent {} referenced by {}", p, id));
            }
        }
    }

    // Every branch head must exist.
    let branches: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT name, head FROM branches")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head) in &branches {
        if let Some(h) = head {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM changesets WHERE id = ?1",
                    rusqlite::params![h],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                out.push(format!(
                    "MISSING changeset {} is the head of branch {}",
                    h, name
                ));
            }
        }
    }

    // A sidecar object's file must be beside the database. Without a root there is no place to look,
    // and a backup file holds every object inside itself, so the check is skipped rather than passed.
    if let Some(root) = root {
        let outside: Vec<String> = {
            let mut stmt = conn.prepare("SELECT hash FROM objects WHERE sidecar = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let gone: Vec<&String> = outside
            .iter()
            .filter(|h| !sidecar::holds(root, h))
            .collect();
        if !gone.is_empty() {
            out.push(format!(
                "MISSING {} of {} sidecar object(s) have no file beside the database",
                gone.len(),
                outside.len()
            ));
        }
    }

    // The maintained tree must equal a rebuild from history.
    let with_heads: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT name, head FROM branches WHERE head IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head) in &with_heads {
        let mut expected = tree_at(conn, head)?;
        let mut stored: Vec<(String, String)> = {
            let mut stmt =
                conn.prepare("SELECT path, hash FROM branch_tree WHERE branch = ?1 ORDER BY path")?;
            let rows = stmt.query_map(rusqlite::params![name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        stored.sort();
        expected.sort();
        if stored != expected {
            let missing = expected.iter().filter(|e| !stored.contains(e)).count();
            let extra = stored.iter().filter(|s| !expected.contains(s)).count();
            out.push(format!(
                "STALE tree for branch {}: {} row(s) stored, {} in history ({} missing, {} extra)",
                name,
                stored.len(),
                expected.len(),
                missing,
                extra
            ));
        }
    }
    Ok(out)
}

pub fn tree_at(conn: &Connection, changeset: &str) -> ZResult<Vec<(String, String)>> {
    Ok(tree_at_with_mode(conn, changeset)?
        .into_iter()
        .map(|(p, h, _)| (p, h))
        .collect())
}

/// The tree at a changeset, with the mode each file carried.
///
/// A mode is not content. A script marked executable and the same script without the bit are the same
/// bytes and different trees to git, and a reader that only sees paths and hashes cannot tell them
/// apart. `100644` is the default for a row written before the column existed, which is what those
/// rows are.
pub fn tree_at_with_mode(
    conn: &Connection,
    changeset: &str,
) -> ZResult<Vec<(String, String, String)>> {
    let reachable = ancestors_bfs(conn, changeset)?;
    if reachable.is_empty() {
        return Ok(Vec::new());
    }

    // Walk the reachable changesets newest first, taking the first row seen for
    // each path. The order is the distance from `changeset`, not the clock:
    // changesets made in one burst share a timestamp to the second, and ordering
    // by time then picks an arbitrary winner. That produced a wrong tree whenever
    // a path was deleted and later added back in the same second, which is what a
    // test caught.
    //
    // `ancestors_bfs` returns the id itself first, then its parents level by
    // level, so the index is the distance from the head.
    let rank: std::collections::HashMap<&str, usize> = reachable
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();

    let placeholders = in_placeholders(reachable.len());
    let sql = format!(
        "SELECT cf.changeset_id, cf.path, cf.new_hash, COALESCE(cf.mode, '100644')
         FROM changeset_files cf WHERE cf.changeset_id IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = reachable
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Nearest to the head wins. A null hash is a deletion, which also wins over
    // anything older for that path.
    let mut seen: std::collections::HashMap<String, (usize, Option<String>, String)> =
        std::collections::HashMap::new();
    for (cs, path, hash, mode) in rows {
        let d = match rank.get(cs.as_str()) {
            Some(d) => *d,
            None => continue,
        };
        match seen.get(&path) {
            Some((best, _, _)) if *best <= d => {}
            _ => {
                seen.insert(path, (d, hash, mode));
            }
        }
    }

    let mut files: Vec<(String, String, String)> = seen
        .into_iter()
        .filter_map(|(p, (_, h, m))| h.map(|hash| (p, hash, m)))
        .collect();
    files.sort();
    Ok(files)
}

/// Whether a table exists.
///
/// `gc` reads tables that only exist once their feature has been used, and preparing a statement
/// against a table that is not there is an error rather than an empty result.
fn table_exists(conn: &Connection, name: &str) -> ZResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Count the SQLite statements `f` runs against `conn`.
///
/// `write-flat`'s property is about `commit_one`, which lives in `repo`, so that count test has to be
/// written there. Exposing one counter beats copying it: two copies means two places to get the
/// `trace_v2` signature wrong, and they drift.
///
/// `SQLITE_TRACE_STMT` fires when a prepared statement begins running, so a prepared-and-reused
/// statement counts once per execution - which is the quantity the claim is about.
#[cfg(test)]
pub(crate) fn statements_during_for_test(
    conn: &mut Connection,
    f: impl FnOnce(&Connection),
) -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    // A `fn` item, not a capturing closure: `trace_v2` wants `FnMut` over a borrowed event and a
    // capturing closure fails the "general enough" bound.
    fn count_one(_event: rusqlite::trace::TraceEvent<'_>) {
        COUNT.fetch_add(1, AtomicOrdering::Relaxed);
    }
    COUNT.store(0, AtomicOrdering::Relaxed);
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(count_one as fn(rusqlite::trace::TraceEvent<'_>)),
    )
    .unwrap();
    f(conn);
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        None::<fn(rusqlite::trace::TraceEvent<'_>)>,
    )
    .unwrap();
    COUNT.load(AtomicOrdering::Relaxed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Every source file in this tier, for the check that reads them.
    ///
    /// Spelled out because `include_str!` is the only way to read a file at compile time and it takes a
    /// literal path. Adding a module here means adding a line, and forgetting to means the new file is
    /// the one that gets the cycle - which is why the test also checks the count against the declarations
    /// in mod.rs.

    #[test]
    fn integrity_problems_reports_an_object_whose_bytes_no_longer_hash_to_its_name() {
        // #335: `objects.hash` was trusted as an identifier and never re-derived, so replacing an
        // object's bytes left the row, the changeset, the signature and every reader untouched -
        // `show`, `verify`, `fsck` and `checkout` all reported success while serving altered content,
        // and `backup` shipped the altered store to a machine that verified it again.
        let conn = mem_db();
        let hash = put_object(&conn, "noi-dung-goc\n").unwrap();

        // Positive control first: an untouched store is clean.
        let clean = integrity_problems(&conn, None).unwrap();
        assert!(
            clean.is_empty(),
            "an untouched store must report no problems: {clean:?}"
        );

        // Now alter the stored bytes and leave every hash column alone, which is exactly the shape
        // an attacker with write access to the database would produce.
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT content FROM objects WHERE hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .unwrap();
        let compressed: i64 = conn
            .query_row(
                "SELECT compressed FROM objects WHERE hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .unwrap();
        let raw = if compressed == 1 {
            crate::store::decompress_bytes(&blob).unwrap()
        } else {
            blob
        };
        let altered = String::from_utf8(raw).unwrap().replace("goc", "DA-SUA");
        conn.execute(
            "UPDATE objects SET content = ?1 WHERE hash = ?2",
            params![altered.as_bytes(), hash],
        )
        .unwrap();

        let problems = integrity_problems(&conn, None).unwrap();
        assert_eq!(
            problems.len(),
            1,
            "exactly one problem must be reported: {problems:?}"
        );
        assert!(
            problems[0].contains("CORRUPT object"),
            "the problem must be named as corruption: {}",
            problems[0]
        );
        assert!(
            problems[0].contains(&hash),
            "the problem must name the object so a user can find it: {}",
            problems[0]
        );

        // And the reader still serves the altered bytes, which is why detection has to be explicit:
        // nothing about the read path can tell.
        assert_eq!(
            String::from_utf8(get_object_bytes(&conn, &hash).unwrap()).unwrap(),
            "noi-dung-DA-SUA\n",
            "the point of the bug: the altered content is served without complaint"
        );
    }

    #[test]
    fn integrity_problems_is_silent_about_a_store_it_cannot_fault() {
        // The other half of a useful check: it must not cry wolf. Several objects, a binary blob, an
        // empty file and a long text all hash to their names, and the check has to agree.
        let conn = mem_db();
        let bodies: Vec<Vec<u8>> = vec![
            b"plain text\n".to_vec(),
            b"".to_vec(),
            vec![0u8, 159, 146, 150, 255, 0, 12],
            "a".repeat(200_000).into_bytes(),
        ];
        for body in &bodies {
            let h = put_object_bytes(&conn, body).unwrap();
            assert_eq!(h, hash_bytes(body), "the fixture itself must be honest");
        }
        let problems = integrity_problems(&conn, None).unwrap();
        assert!(
            problems.is_empty(),
            "no object was altered, so nothing may be reported: {problems:?}"
        );
    }

    #[test]
    fn a_delta_that_cannot_be_recorded_leaves_no_half_written_object() {
        // #116: the `objects` row and its `deltas` row were two statements outside a transaction. A
        // failure between them left the hash present with an empty body and no delta to rebuild it,
        // so every later read of that hash failed as corrupt and the content was unreachable.
        //
        // The trigger refuses the `deltas` insert, which is exactly that failure point.
        let conn = crate::store::tests::mem_db();
        // A body long enough that a compressed whole object is far bigger than a one-line delta, so
        // the delta path is chosen and the margin is met.
        let base: String = (0..400)
            .map(|i| format!("line {i} of the base\n"))
            .collect();
        put_object(&conn, &base).unwrap();

        conn.execute_batch(
            "CREATE TRIGGER fail_delta BEFORE INSERT ON deltas
             BEGIN SELECT RAISE(ABORT, 'forced delta failure'); END",
        )
        .unwrap();

        // Close enough to the base that the delta path is taken and the margin is met.
        let edited = base.replace("line 200 of the base", "line 200 was CHANGED");
        let err = put_object_against(&conn, &edited, Some(&hash_content(&base)))
            .expect_err("the trigger must fail the write");
        assert!(
            format!("{err}").contains("forced delta failure"),
            "the trigger's own error must surface, got: {err}"
        );

        // The hash is either absent, or present and readable. What it must not be is present with
        // nothing behind it, which is the state the bug created.
        let h = hash_content(&edited);
        let row: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM objects WHERE hash = ?1",
                rusqlite::params![h],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        if row.is_some() {
            let body = get_object_bytes(&conn, &h)
                .expect("a recorded hash must be readable, not a bodyless row");
            assert_eq!(String::from_utf8_lossy(&body), edited);
        }
    }

    #[test]
    fn hash_file_matches_hash_bytes_on_the_same_content() {
        // The streaming hash exists so a large file never has to be resident. It has to agree with
        // `hash_bytes` exactly or an object stored by one path would be unfindable by the other,
        // and the two are deliberately written differently: one whole-buffer, one block loop.
        let dir = std::env::temp_dir().join(format!("gesta-hashfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.bin");
        // Spans several read blocks so the loop is exercised more than once.
        let body: Vec<u8> = (0..(5u32 << 20)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &body).unwrap();
        assert_eq!(
            hash_file(&p).unwrap(),
            hash_bytes(&body),
            "the block hash and the whole-buffer hash disagreed, so an object would be stored under one name and looked for under another"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storing_a_large_file_from_disk_round_trips_and_keeps_the_body_out_of_the_row() {
        // `put_object_from_file` is the path a large asset takes. Two things can go wrong that no
        // other test would see: the copy can truncate, and the row can end up carrying the body
        // instead of a sidecar pointer, which is the memory the whole path exists to avoid.
        //
        // The connection is opened on a file rather than in memory because the sidecar root is
        // derived from the database path: an in-memory connection has no root, so the function
        // would fall back to reading the file whole and the test would pass without ever
        // exercising the block copy.
        let dir = std::env::temp_dir().join(format!("gesta-putfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let conn = Connection::open(dir.join(".gesta").join("gesta.db")).unwrap();
        init_schema(&conn).unwrap();

        let p = dir.join("big.bin");
        let body: Vec<u8> = (0..(5u32 << 20)).map(|i| (i % 241) as u8).collect();
        std::fs::write(&p, &body).unwrap();
        let hash = hash_file(&p).unwrap();

        let stored = put_object_from_file(&conn, &p, &hash, body.len() as u64).unwrap();
        assert_eq!(stored, hash, "storing from a file changed the hash");
        assert_eq!(
            get_object_bytes(&conn, &hash).unwrap(),
            body,
            "the body read back differed from what was on disk, so the block copy lost or reordered bytes"
        );
        let (size, sidecar): (i64, Option<i64>) = conn
            .query_row(
                "SELECT size, sidecar FROM objects WHERE hash = ?1",
                params![hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            size as usize,
            body.len(),
            "the recorded size was not the body's"
        );
        assert_eq!(
            sidecar,
            Some(1),
            "a 5 MB body was not sent to a sidecar, so it was held in the row this path exists to keep free"
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_storage_layer_does_not_call_up_into_the_repository_layer() {
        // A cycle inside one crate is invisible to the compiler: `store` calling `repo` while `repo`
        // calls `store` builds cleanly and only shows up by reading use lines.
        //
        // This reads every file in the tier, discovered from the declarations in this file rather
        // than from a list written out by hand. The hand-written version was wrong the moment it was
        // committed - it was missing `vfs.rs` - and would have failed silently in exactly the case it
        // exists for. A guard that needs updating alongside the thing it guards is a guard that stops
        // guarding without saying so.
        let upward = concat!("crate", "::repo", "::");
        let fixture = concat!("crate", "::repo", "::tests");
        // A test that checks what the storage layer wrote may read it back through the layer above,
        // because a test is not a caller of the product. Named explicitly so adding one is a
        // deliberate act that shows up in review, rather than a pattern that silently permits it.
        let test_readback = concat!("use crate", "::repo", "::committed_tree;");

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store");
        let mut checked = 0usize;
        let mut offenders: Vec<String> = Vec::new();

        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {}", dir.display(), e))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
            .collect();
        paths.sort();

        assert!(
            paths.len() >= 10,
            "found only {} source file(s) in {}, so this check is reading the wrong directory",
            paths.len(),
            dir.display()
        );

        for path in paths {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            checked += 1;
            for (n, line) in src.lines().enumerate() {
                if line.contains(upward) && !line.contains(fixture) && !line.contains(test_readback)
                {
                    offenders.push(format!("{}:{}: {}", name, n + 1, line.trim()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "the storage layer reaches up into repo ({} files read):\n  {}",
            checked,
            offenders.join("\n  ")
        );
    }

    fn gc_disk_repo(label: &str) -> (Connection, PathBuf) {
        let root = std::env::temp_dir().join(format!("gesta-gc-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".gesta")).unwrap();
        let conn = open_db(&root).unwrap();
        init_schema(&conn).unwrap();
        (conn, root)
    }

    #[test]
    fn gc_reclaims_dead_sidecars_and_preserves_live_bytes() {
        let (conn, root) = gc_disk_repo("sidecars");
        let live = vec![7; sidecar::INLINE_LIMIT];
        let dead = vec![9; sidecar::INLINE_LIMIT];
        let kept = put_object_bytes(&conn, &live).unwrap();
        let removed = put_object_bytes(&conn, &dead).unwrap();
        conn.execute(
            "INSERT INTO snapshot_objects(path, hash) VALUES('live.bin', ?1)",
            params![kept],
        )
        .unwrap();
        assert_eq!(sidecar::stats(&root), (2, (live.len() + dead.len()) as u64));
        assert_eq!(gc(&conn).unwrap(), 1);
        assert!(!sidecar::path_for(&root, &removed).unwrap().exists());
        assert_eq!(sidecar::stats(&root), (1, live.len() as u64));
        assert_eq!(get_object_bytes(&conn, &kept).unwrap(), live);
        assert_eq!(gc(&conn).unwrap(), 0);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_accepts_missing_dead_sidecars_but_reports_removal_errors() {
        let (conn, root) = gc_disk_repo("sidecar-errors");
        let hash = put_object_bytes(&conn, &vec![3; sidecar::INLINE_LIMIT]).unwrap();
        let path = sidecar::path_for(&root, &hash).unwrap();
        std::fs::remove_file(&path).unwrap();
        // A directory at the object path fails removal on Linux and Windows, even as root.
        std::fs::create_dir(&path).unwrap();
        assert!(gc(&conn).is_err());
        assert_eq!(
            object_count(&conn).unwrap(),
            1,
            "keep the row so cleanup can retry"
        );
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(gc(&conn).unwrap(), 1);
        assert_eq!(gc(&conn).unwrap(), 0);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_waits_for_a_writer_before_deleting_sidecars() {
        let (conn, root) = gc_disk_repo("writer");
        let body = vec![5; sidecar::INLINE_LIMIT];
        let hash = put_object_bytes(&conn, &body).unwrap();
        let writer = open_db(&root).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute(
                "INSERT INTO snapshot_objects(path, hash) VALUES('pending.bin', ?1)",
                params![hash],
            )
            .unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        assert!(
            gc(&conn).is_err(),
            "GC must not use a stale live set while a writer owns the lock"
        );
        assert_eq!(get_object_bytes(&conn, &hash).unwrap(), body);
        writer.execute_batch("COMMIT").unwrap();
        assert_eq!(gc(&conn).unwrap(), 0);
        assert_eq!(get_object_bytes(&conn, &hash).unwrap(), body);
        drop(writer);
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_drops_empty_groups_but_preserves_partially_live_groups() {
        let conn = mem_db();
        let hashes: Vec<String> = (0..50)
            .map(|i| {
                let text: String = (0..100)
                    .map(|j| format!("group member {i} source line {j}\n"))
                    .collect();
                put_object(&conn, &text).unwrap()
            })
            .collect();
        assert_eq!(pack::repack(&conn).unwrap(), (1, 50));
        let kept = &hashes[0];
        let bytes = get_object_bytes(&conn, kept).unwrap();
        conn.execute(
            "INSERT INTO snapshot_objects(path, hash) VALUES('member.txt', ?1)",
            params![kept],
        )
        .unwrap();
        assert_eq!(gc(&conn).unwrap(), 49);
        assert_eq!(pack::group_stats(&conn).unwrap().0, 1);
        assert_eq!(get_object_bytes(&conn, kept).unwrap(), bytes);
        conn.execute("DELETE FROM snapshot_objects", []).unwrap();
        assert_eq!(gc(&conn).unwrap(), 1);
        assert_eq!(pack::group_stats(&conn).unwrap(), (0, 0, 0));
        assert_eq!(gc(&conn).unwrap(), 0);
    }

    #[test]
    fn gc_preserves_snapshot_objects() {
        let conn = mem_db();
        let kept = put_object(&conn, "shallow snapshot body").unwrap();
        conn.execute(
            "INSERT INTO snapshot_objects(path, hash) VALUES('shallow.txt', ?1)",
            params![kept],
        )
        .unwrap();
        put_object(&conn, "unreachable snapshot neighbour").unwrap();
        assert_eq!(gc(&conn).unwrap(), 1);
        assert_eq!(get_object(&conn, &kept).unwrap(), "shallow snapshot body");
        assert_eq!(gc(&conn).unwrap(), 0);
    }

    #[test]
    fn gc_preserves_transitive_delta_bases() {
        let conn = mem_db();
        let base: String = (0..500)
            .map(|n| format!("unique source line {n:08}\n"))
            .collect();
        let middle = format!("{base}middle edit\n");
        let tip = format!("{middle}final edit\n");
        let h0 = put_object(&conn, &base).unwrap();
        let h1 = put_object_against(&conn, &middle, Some(&h0)).unwrap();
        let h2 = put_object_against(&conn, &tip, Some(&h1)).unwrap();
        assert_eq!(
            delta_depth(&conn, &h2).unwrap(),
            2,
            "fixture needs two real deltas"
        );
        crate::repo::tests::commit_files(&conn, "gc-delta", "gc delta", &[("tip.txt", &tip)]);
        put_object(&conn, "unreachable unrelated body").unwrap();
        assert_eq!(gc(&conn).unwrap(), 1);
        assert_eq!(get_object(&conn, &h0).unwrap(), base);
        assert_eq!(get_object(&conn, &h1).unwrap(), middle);
        assert_eq!(get_object(&conn, &h2).unwrap(), tip);
        assert_eq!(gc(&conn).unwrap(), 0);
    }

    #[test]
    fn gc_terminates_on_a_reachable_delta_cycle() {
        let conn = mem_db();
        let h0 = put_object(&conn, "cycle first body").unwrap();
        let h1 = put_object(&conn, "cycle second body").unwrap();
        for (hash, base) in [(&h0, &h1), (&h1, &h0)] {
            conn.execute(
                "INSERT INTO deltas(hash, base, payload, created_at) VALUES(?1, ?2, X'', 0)",
                params![hash, base],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO snapshot_objects(path, hash) VALUES('cycle.txt', ?1)",
            params![h0],
        )
        .unwrap();
        // The fixture is corrupt; GC must terminate without deleting either live dependency.
        assert_eq!(gc(&conn).unwrap(), 0);
        assert_eq!(object_count(&conn).unwrap(), 2);
    }

    #[test]
    fn gc_collects_unreachable_delta_chains() {
        let conn = mem_db();
        let base: String = (0..500)
            .map(|n| format!("unique orphan line {n:08}\n"))
            .collect();
        let h0 = put_object(&conn, &base).unwrap();
        let h1 = put_object_against(&conn, &format!("{base}orphan edit\n"), Some(&h0)).unwrap();
        assert_eq!(delta_depth(&conn, &h1).unwrap(), 1);
        assert_eq!(gc(&conn).unwrap(), 2);
        assert_eq!(object_count(&conn).unwrap(), 0);
    }

    #[test]
    fn gc_removes_unreachable_objects() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        crate::repo::tests::commit_files(&conn, "gc1", "gc1", &[("a.txt", "v1\n")]);
        set_branch_head(&conn, "main", "gc1").unwrap();
        set_state(&conn, "current_branch", "main").unwrap();

        // An object nothing points at.
        put_object(&conn, "unreachable content").unwrap();
        let before = object_count(&conn).unwrap();

        let deleted = gc(&conn).unwrap();
        assert_eq!(deleted, 1, "the unreachable object was not removed");
        assert_eq!(object_count(&conn).unwrap() + deleted as i64, before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    pub fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn hash_is_stable_and_distinct() {
        assert_eq!(hash_content("abc"), hash_content("abc"));
        assert_ne!(hash_content("abc"), hash_content("abd"));
        assert_eq!(hash_content("").len(), 64);
    }

    #[test]
    fn object_store_roundtrip_is_compressed_and_deduped() {
        let conn = mem_db();
        let text = "line of text\n".repeat(500);
        let a = put_object(&conn, &text).unwrap();
        let b = put_object(&conn, &text).unwrap();
        assert_eq!(a, b);
        assert_eq!(get_object(&conn, &a).unwrap(), text);

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "duplicate content must not create a second object");

        let (raw, stored) = object_store_bytes(&conn).unwrap();
        assert!(
            stored * 3 < raw,
            "repetitive text must compress at least 3x, got {} -> {}",
            raw,
            stored
        );
    }

    #[test]
    fn tiny_objects_are_stored_uncompressed_when_zlib_would_grow_them() {
        let conn = mem_db();
        let tiny = "a\n";
        let h = put_object(&conn, tiny).unwrap();
        assert_eq!(get_object(&conn, &h).unwrap(), tiny);

        let (flag, stored): (i64, i64) = conn
            .query_row(
                "SELECT compressed, LENGTH(content) FROM objects WHERE hash = ?1",
                params![h],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(flag, 0, "a 2-byte object must not be stored compressed");
        assert!(
            stored <= tiny.len() as i64 + 2,
            "stored size {} should not exceed the raw size {}",
            stored,
            tiny.len()
        );
    }

    #[test]
    fn missing_object_is_an_error_not_empty_string() {
        let conn = mem_db();
        let err = get_object(&conn, "deadbeef").unwrap_err().to_string();
        assert!(err.contains("not found"), "got: {}", err);
    }

    #[test]
    fn repo_state_upsert() {
        let conn = mem_db();
        set_state(&conn, "current_branch", "main").unwrap();
        assert_eq!(current_branch(&conn).unwrap(), "main");
        set_state(&conn, "current_branch", "dev").unwrap();
        assert_eq!(current_branch(&conn).unwrap(), "dev");
    }

    #[test]
    fn unknown_branch_is_an_error() {
        let conn = mem_db();
        assert!(reachable_from(&conn, "nope").is_err());
        assert!(branch_head(&conn, "nope").is_err());
    }

    #[test]
    fn ids_are_unique_across_calls() {
        let conn = mem_db();
        let a = new_changeset_id(&conn, "m", "a").unwrap();
        let b = new_changeset_id(&conn, "m", "a").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 14, "cs + 12 hex");
    }

    fn add_cs(conn: &Connection, id: &str, p1: Option<&str>, p2: Option<&str>) {
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
             VALUES(?1,'m','a',1,'committed',?2,?3,'main')",
            params![id, p1, p2],
        )
        .unwrap();
    }

    #[test]
    fn reachability_follows_both_parents_of_a_merge() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs1"), None);
        add_cs(&conn, "cs4", Some("cs2"), Some("cs3"));
        conn.execute("UPDATE branches SET head='cs4' WHERE name='main'", [])
            .unwrap();

        let r = reachable_from(&conn, "main").unwrap();
        for id in ["cs1", "cs2", "cs3", "cs4"] {
            assert!(
                r.contains(&id.to_string()),
                "{} must stay reachable, got {:?}",
                id,
                r
            );
        }
    }

    #[test]
    fn merge_base_finds_the_fork_point() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs2"), None);
        add_cs(&conn, "cs4", Some("cs2"), None);
        assert_eq!(
            merge_base(&conn, "cs3", "cs4").unwrap().as_deref(),
            Some("cs2")
        );
        assert_eq!(
            merge_base(&conn, "cs2", "cs3").unwrap().as_deref(),
            Some("cs2")
        );
    }

    #[test]
    fn merge_base_returns_every_best_candidate_of_a_criss_cross() {
        // The shape: two branches each merge the other, twice. A and B both descend from base; A1 and
        // B1 merge each other's opposite, and then A2 and B2 merge those merges. There is no single
        // best common ancestor - A1 and B1 are both best, because neither reaches the other - and any
        // merge that picks one of them silently discards the other's changes.
        //
        //     base -> A1 --\        A2 --\
        //              \   M1 ---...  \    M2
        //               \  /            \  /
        //     base -> B1 --/        B2 --/
        //
        // git reports both and lets a recursive merge build a virtual base from them. Returning one
        // is what makes a criss-cross merge lose a change without reporting a conflict.
        // Both merges have to be reached by both sides, or it is not a criss-cross. The first
        // version of this fixture chained a2 -> m1a -> a1 and b2 -> m1b -> b1 separately, which left
        // m1a unreachable from b2; the two answers that came back, a1 and b1, were correct for that
        // graph. The shape only has two bases when each merge is an ancestor of both tips.
        let conn = mem_db();
        add_cs(&conn, "base", None, None);
        add_cs(&conn, "a1", Some("base"), None);
        add_cs(&conn, "b1", Some("base"), None);
        add_cs(&conn, "m1a", Some("a1"), Some("b1"));
        add_cs(&conn, "m1b", Some("b1"), Some("a1"));
        add_cs(&conn, "a2", Some("m1a"), Some("m1b"));
        add_cs(&conn, "b2", Some("m1b"), Some("m1a"));

        let bases = merge_bases(&conn, "a2", "b2").unwrap();
        let mut got: Vec<String> = bases;
        got.sort();
        assert_eq!(
            got,
            vec!["m1a".to_string(), "m1b".to_string()],
            "a criss-cross has two best common ancestors; returning fewer loses a change"
        );
    }

    /// Run `f` and return how many statements SQLite started while it ran.
    ///
    /// The count is the property that matters and a clock is not: `walk_ancestors` takes a
    /// `&ParentMap`, not a `&Connection`, so the walk cannot read a row even if someone tried. What
    /// a later change can do is put the read back around the loop, and that shows up as a count
    /// that grows with the history rather than a duration that may or may not be separable from
    /// scheduling noise.
    ///
    /// `SQLITE_TRACE_STMT` fires when a prepared statement begins running. `parent_map` prepares one
    /// statement and reads all rows from it, so a walk over a map costs one, no matter how many rows
    /// the map has.
    fn statements_during(conn: &mut Connection, f: impl FnOnce(&Connection)) -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        // A function pointer rather than a closure that captures: `trace_v2` requires `FnMut` over a
        // borrowed event, and a capturing closure cannot satisfy the "general enough" bound. A
        // process-wide counter sidesteps the lifetime without changing what is counted, and these
        // tests run one at a time under the harness's default serialisation of a single test thread.
        fn count_one(_event: rusqlite::trace::TraceEvent<'_>) {
            COUNT.fetch_add(1, AtomicOrdering::Relaxed);
        }
        COUNT.store(0, AtomicOrdering::Relaxed);
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_one as fn(rusqlite::trace::TraceEvent<'_>)),
        )
        .unwrap();
        f(conn);
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            None::<fn(rusqlite::trace::TraceEvent<'_>)>,
        )
        .unwrap();
        COUNT.load(AtomicOrdering::Relaxed)
    }

    #[test]
    fn answering_a_merge_base_reads_the_parent_table_once_whatever_the_history_length() {
        // The defect this guards: `merge_bases` used to call `ancestors` once per input and then once
        // per common ancestor, and `ancestors` read the parent row of every changeset it visited.
        // On a linear history of n that is n queries plus n more per candidate. It passed every
        // correctness test in this file, because the answer was right; only the number of reads was
        // wrong, and nothing counted them.
        //
        // Two histories of very different lengths, the same question asked of each: forked, so the
        // answer is a real merge base and not the fast-forward the generations settle in one query.
        // A count that grows with the second history is the N+1 shape coming back.
        fn count_for(n: usize) -> usize {
            let mut conn = mem_db();
            // A line, then a fork at the tip and a re-merge: `merge_bases` is reached by the two
            // branch heads, and neither contains the other, so the generation shortcut declines.
            for i in 0..n {
                let id = format!("c{i}");
                let parent = if i == 0 {
                    None
                } else {
                    Some(format!("c{}", i - 1))
                };
                add_cs(&conn, &id, parent.as_deref(), None);
            }
            add_cs(&conn, "left", Some(&format!("c{}", n - 1)), None);
            add_cs(&conn, "right", Some(&format!("c{}", n - 1)), None);
            statements_during(&mut conn, |c| {
                let _ = merge_bases(c, "left", "right");
            })
        }

        let small = count_for(8);
        let large = count_for(400);

        assert_eq!(
            small, large,
            "reading the parent table must not be repeated per changeset: {small} statements over a \
             short history, {large} over one fifty times longer"
        );
        // One read of the parent map, plus the two generation lookups the shortcut tries first.
        // Stated as a ceiling rather than an equality so a change that removes a lookup passes, and
        // one that adds a per-row read does not.
        assert!(
            large <= 5,
            "a merge base over 400 changesets took {large} statements; the parent table should be \
             read once"
        );
    }

    #[test]
    fn merge_bases_returns_one_for_an_ordinary_fork() {
        // The common case must not start returning two: one base is what a three-way merge needs, and
        // a list of one is the honest shape of that.
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs1"), None);
        assert_eq!(
            merge_bases(&conn, "cs2", "cs3").unwrap(),
            vec!["cs1".to_string()]
        );
    }

    #[test]
    fn is_ancestor_answers_both_directions() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs2"), None);

        assert!(
            is_ancestor(&conn, "cs1", "cs3").unwrap(),
            "root reaches the tip"
        );
        assert!(
            !is_ancestor(&conn, "cs3", "cs1").unwrap(),
            "the tip is not before the root"
        );
        assert!(
            is_ancestor(&conn, "cs3", "cs3").unwrap(),
            "a point is its own ancestor"
        );
    }

    #[test]
    fn is_ancestor_says_no_for_two_siblings() {
        // A sibling has a lower generation than nothing and is not on the other's chain. The
        // generation shortcut must not turn this into a false "yes".
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs1"), None);
        assert!(!is_ancestor(&conn, "cs2", "cs3").unwrap());
        assert!(!is_ancestor(&conn, "cs3", "cs2").unwrap());
    }

    #[test]
    fn is_ancestor_follows_the_second_parent_of_a_merge() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs1"), None);
        add_cs(&conn, "cs4", Some("cs2"), Some("cs3"));
        // cs3 is only reachable through the merge's second parent, which is the parent a
        // first-parent walk would miss.
        assert!(is_ancestor(&conn, "cs3", "cs4").unwrap());
    }

    #[test]
    fn a_null_generation_still_answers_correctly() {
        // A repository migrated before the column was filled has no numbers. The shortcut cannot
        // fire, so the answer has to come from the walk, and the walk has to still be right.
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        assert!(is_ancestor(&conn, "cs1", "cs2").unwrap());
        assert!(!is_ancestor(&conn, "cs2", "cs1").unwrap());
    }

    #[test]
    fn merge_bases_of_a_fast_forward_is_the_older_point() {
        // One side already contains the other. The answer is the ancestor itself, and it has to be
        // reached without building the reachability map of every common ancestor.
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", Some("cs1"), None);
        add_cs(&conn, "cs3", Some("cs2"), None);
        conn.execute("UPDATE changesets SET generation = 1 WHERE id = 'cs1'", [])
            .unwrap();
        conn.execute("UPDATE changesets SET generation = 2 WHERE id = 'cs2'", [])
            .unwrap();
        conn.execute("UPDATE changesets SET generation = 3 WHERE id = 'cs3'", [])
            .unwrap();
        assert_eq!(
            merge_bases(&conn, "cs1", "cs3").unwrap(),
            vec!["cs1".to_string()]
        );
        assert_eq!(
            merge_bases(&conn, "cs3", "cs1").unwrap(),
            vec!["cs1".to_string()]
        );
    }

    #[test]
    fn merge_bases_of_unrelated_history_is_empty() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", None, None);
        assert!(merge_bases(&conn, "cs1", "cs2").unwrap().is_empty());
    }

    #[test]
    fn merge_base_of_unrelated_history_is_none() {
        let conn = mem_db();
        add_cs(&conn, "cs1", None, None);
        add_cs(&conn, "cs2", None, None);
        assert_eq!(merge_base(&conn, "cs1", "cs2").unwrap(), None);
    }

    #[test]
    fn latest_hash_follows_reachability_not_globally_newest() {
        let conn = mem_db();
        let h1 = put_object(&conn, "v1\n").unwrap();
        let h2 = put_object(&conn, "v2\n").unwrap();

        for (id, parent, hash, ver) in [("cs1", None, &h1, 1i64), ("cs2", Some("cs1"), &h2, 2)] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
                 VALUES(?1, 'm', 'a', 1, 'committed', ?2, 'main')",
                params![id, parent],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
                 VALUES('f.txt', ?1, ?2, ?3, 1)",
                params![ver, hash, id],
            )
            .unwrap();
            // hash_at_point reads changeset_files, so a fixture that only wrote
            // file_versions would not describe a real repository.
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
                 VALUES(?1, 'f.txt', NULL, ?2)",
                params![id, hash],
            )
            .unwrap();
        }

        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", [])
            .unwrap();
        assert_eq!(
            latest_hash(&conn, "f.txt", "main").unwrap().as_deref(),
            Some(h2.as_str())
        );

        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('pinned', 'cs1', 0, 'main')",
            [],
        )
        .unwrap();
        assert_eq!(
            latest_hash(&conn, "f.txt", "pinned").unwrap().as_deref(),
            Some(h1.as_str())
        );
        assert_eq!(latest_hash(&conn, "absent.txt", "main").unwrap(), None);
    }

    #[test]
    fn a_busy_error_is_recognised() {
        // Shaped like the error SQLite raises when another process holds the
        // write lock. Recognising it is what lets the retry loop help.
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5), // SQLITE_BUSY
            Some("database is locked".to_string()),
        );
        assert!(is_busy(&busy));

        let other = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(1), // SQLITE_ERROR
            Some("no such table".to_string()),
        );
        assert!(!is_busy(&other));

        assert!(!is_busy(&rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn with_retry_passes_a_success_through() {
        let mut calls = 0;
        let v = with_retry(|| {
            calls += 1;
            Ok(7)
        })
        .unwrap();
        assert_eq!(v, 7);
        assert_eq!(calls, 1);
    }

    #[test]
    fn with_retry_gives_up_on_an_error_that_is_not_busy() {
        let mut calls = 0;
        let r: ZResult<()> = with_retry(|| {
            calls += 1;
            Err(bad("no such column".to_string()))
        });
        assert!(r.is_err());
        assert_eq!(calls, 1, "a non-busy error must not be retried");
    }

    #[test]
    fn with_retry_retries_a_busy_error_then_succeeds() {
        let mut calls = 0;
        let v = with_retry(|| {
            calls += 1;
            if calls < 4 {
                Err(Box::new(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(5),
                    Some("database is locked".to_string()),
                ))
                    as Box<dyn std::error::Error + Send + Sync>)
            } else {
                Ok(calls)
            }
        })
        .unwrap();
        assert_eq!(v, 4);
        assert_eq!(calls, 4);
    }

    #[test]
    fn resolve_rev_accepts_a_branch_name() {
        // `show main:file.txt` passes "main". A resolver that only knew
        // changeset ids answered "file does not exist" for a file that was in
        // the head tree, which is how this was found.
        let conn = mem_db();
        let h = put_object(&conn, "x\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs1', 'm', 'a', 1, 'committed', NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES('cs1', 'x.txt', NULL, ?1)",
            params![h],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", [])
            .unwrap();

        assert_eq!(resolve_rev(&conn, "main").unwrap().as_deref(), Some("cs1"));
    }

    #[test]
    fn resolve_rev_accepts_a_changeset_id_and_a_tag() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('cs9', 'm', 'a', 1, 'committed', NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags(name, changeset_id, created_at) VALUES('v1', 'cs9', 1)",
            [],
        )
        .unwrap();

        assert_eq!(resolve_rev(&conn, "cs9").unwrap().as_deref(), Some("cs9"));
        assert_eq!(resolve_rev(&conn, "v1").unwrap().as_deref(), Some("cs9"));
    }

    #[test]
    fn resolve_rev_reports_an_unknown_name() {
        let conn = mem_db();
        assert_eq!(resolve_rev(&conn, "nowhere").unwrap(), None);
    }

    #[test]
    fn resolve_rev_does_not_interpret_prefixes_as_like_patterns() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, branch)
             VALUES('csabcd1234', 'm', 'a', 1, 'committed', 'main')",
            [],
        )
        .unwrap();
        for rev in ["csab_d", "csab%4", "csab!d"] {
            assert_eq!(resolve_rev(&conn, rev).unwrap(), None, "{rev}");
        }
    }

    #[test]
    fn resolve_rev_accepts_literal_pattern_characters_in_prefixes() {
        let conn = mem_db();
        for id in ["cs!_%abc", "cs!XYZabc"] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, branch)
                 VALUES(?1, 'm', 'a', 1, 'committed', 'main')",
                params![id],
            )
            .unwrap();
        }
        for rev in ["cs!_", "cs!_%", "cs!_%a"] {
            assert_eq!(
                resolve_rev(&conn, rev).unwrap().as_deref(),
                Some("cs!_%abc"),
                "{rev}"
            );
        }
    }

    #[test]
    fn resolve_rev_preserves_plain_prefixes_and_real_ambiguity() {
        let conn = mem_db();
        for (id, commit) in [
            ("csAbÉcd123", "abcdef0123456789"),
            ("csdupe123", "fedcba9876543210"),
            ("csdupe456", "0123456789abcdef"),
        ] {
            conn.execute(
                "INSERT INTO changesets(id, message, author, created_at, status, branch, git_commit)
                 VALUES(?1, 'm', 'a', 1, 'committed', 'main', ?2)",
                params![id, commit],
            )
            .unwrap();
        }
        assert_eq!(
            resolve_rev(&conn, "csaBÉ").unwrap().as_deref(),
            Some("csAbÉcd123")
        );
        assert_eq!(
            resolve_rev(&conn, "abcdef0").unwrap().as_deref(),
            Some("csAbÉcd123")
        );
        let error = resolve_rev(&conn, "csdupe").expect_err("two literal prefixes are ambiguous");
        assert!(
            error.to_string().contains("matches 2 changesets"),
            "{error}"
        );
    }

    #[test]
    fn placeholders_are_question_marks_only() {
        assert_eq!(in_placeholders(1), "?");
        assert_eq!(in_placeholders(3), "?,?,?");
        // No value can be hidden in this string, which is the point: it is the
        // only place SQL is assembled.
        assert!(!in_placeholders(5).contains('\''));
        assert!(!in_placeholders(5).contains("--"));
        assert!(!in_placeholders(5).contains(';'));
    }

    #[test]
    fn an_empty_list_matches_nothing_rather_than_failing() {
        // `IN ()` is a syntax error in SQLite, so an empty reachable set has to
        // become NULL and match nothing.
        assert_eq!(in_placeholders(0), "NULL");
    }

    #[test]
    fn a_query_built_with_the_helper_runs() {
        let conn = mem_db();
        let h = put_object(&conn, "x\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','m','a',1,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','p.txt',NULL,?1)",
            params![h],
        )
        .unwrap();

        let ids = ["cs1".to_string()];
        let sql = format!(
            "SELECT COUNT(*) FROM changeset_files WHERE changeset_id IN ({})",
            in_placeholders(ids.len())
        );
        let n: i64 = conn.query_row(&sql, params![ids[0]], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_delta_whose_base_is_itself_a_delta_still_reads_back() {
        // The first edit stores a delta against v0. The second edit stores a
        // delta against the first delta. Reading the second must follow the
        // chain: reading the base's row directly returns an empty body, and the
        // rebuild then fails or produces the wrong bytes. This is regression
        // number one for the delta layer.
        let conn = mem_db();
        let v0: String = (0..200).map(|i| format!("line {}\n", i)).collect();
        let h0 = put_object(&conn, &v0).unwrap();

        let mut lines: Vec<String> = v0.lines().map(|s| s.to_string()).collect();
        lines[100] = "changed once".to_string();
        let v1 = lines.join("\n") + "\n";
        let h1 = put_object_against(&conn, &v1, Some(&h0)).unwrap();

        lines[100] = "changed twice".to_string();
        let v2 = lines.join("\n") + "\n";
        let h2 = put_object_against(&conn, &v2, Some(&h1)).unwrap();

        assert_eq!(get_object(&conn, &h1).unwrap(), v1, "first delta");
        assert_eq!(get_object(&conn, &h2).unwrap(), v2, "nested delta");
        assert_eq!(get_object(&conn, &h0).unwrap(), v0, "base still whole");
    }

    #[test]
    fn delta_depth_propagates_query_errors() {
        let conn = mem_db();
        assert_eq!(delta_depth(&conn, "missing").unwrap(), 0);
        conn.execute_batch("DROP TABLE deltas").unwrap();
        let error = delta_depth(&conn, "missing")
            .expect_err("a failed depth query must not look like a whole object");
        assert!(error.to_string().contains("deltas"), "{error}");
    }

    #[test]
    fn delta_depth_counts_the_whole_chain_in_one_query() {
        // The count is what decides whether an object may be a base, so it has to be right at the
        // ends: a whole object is depth 0, a chain of three is 3, and the cap stops a cycle
        // instead of looping. This replaced a loop that asked the database once per link; the
        // answer has to be identical, because the guard that keeps chains shallow reads it.
        let conn = mem_db();
        let v0: String = (0..200).map(|i| format!("line {}\n", i)).collect();
        let h0 = put_object(&conn, &v0).unwrap();
        assert_eq!(
            delta_depth(&conn, &h0).unwrap(),
            0,
            "a whole object has no chain"
        );

        let mut lines: Vec<String> = v0.lines().map(|s| s.to_string()).collect();
        let mut prev = h0;
        for round in 1..=3 {
            lines[10] = format!("round {}\n", round);
            let body = lines.join("\n") + "\n";
            prev = put_object_against(&conn, &body, Some(&prev)).unwrap();
            assert_eq!(
                delta_depth(&conn, &prev).unwrap(),
                round,
                "chain length after {} edits",
                round
            );
        }
    }

    #[test]
    fn a_cycle_in_the_delta_table_does_not_loop_forever() {
        // A store this program never writes can still be handed to it: a cycle in `deltas` would
        // make an uncapped walk run for ever. The cap has to be inside the query, not around it.
        let conn = mem_db();
        conn.execute(
            "INSERT INTO deltas(hash, base, payload, created_at) VALUES('a', 'b', x'', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO deltas(hash, base, payload, created_at) VALUES('b', 'a', x'', 1)",
            [],
        )
        .unwrap();
        let d = delta_depth(&conn, "a").unwrap();
        assert!(
            d <= MAX_DELTA_DEPTH + 1,
            "a cycle must stop at the cap, got {}",
            d
        );
    }

    #[test]
    fn thirty_successive_edits_all_read_back() {
        // The bench that found the nested-delta defect drove thirty edits. Each
        // one has to read back exactly, not just the last.
        let conn = mem_db();
        let mut lines: Vec<String> = (0..300).map(|i| format!("line {}\n", i)).collect();
        let mut contents = vec![lines.join("")];
        let mut hashes = vec![put_object(&conn, &contents[0]).unwrap()];

        for round in 1..=30 {
            lines[150] = format!("edited round {}\n", round);
            let text = lines.join("");
            let prev = hashes.last().unwrap().clone();
            let h = put_object_against(&conn, &text, Some(&prev)).unwrap();
            contents.push(text);
            hashes.push(h);
        }

        for (i, h) in hashes.iter().enumerate() {
            assert_eq!(
                get_object(&conn, h).unwrap(),
                contents[i],
                "version {} did not read back",
                i
            );
        }
    }

    #[test]
    fn a_corrupt_delta_is_an_error_not_wrong_content() {
        let conn = mem_db();
        let v0: String = (0..100).map(|i| format!("l{}\n", i)).collect();
        let h0 = put_object(&conn, &v0).unwrap();
        let mut lines: Vec<String> = v0.lines().map(|s| s.to_string()).collect();
        lines[50] = "changed".to_string();
        let v1 = lines.join("\n") + "\n";
        let h1 = put_object_against(&conn, &v1, Some(&h0)).unwrap();

        // Corrupt the payload in place.
        conn.execute(
            "UPDATE deltas SET payload = X'00' WHERE hash = ?1",
            params![h1],
        )
        .unwrap();
        assert!(
            get_object(&conn, &h1).is_err(),
            "corruption must not be silent"
        );
    }

    #[test]
    fn a_long_edit_history_keeps_the_delta_chain_shallow() {
        // Without a cap the chain grows by one link per edit, so reading the
        // newest version costs a decompression per edit that came before it. A
        // 200 line file with 120 versions took 131 ms to commit and the chain was
        // 120 links deep. The cap restarts the chain periodically, and this test
        // pins both the depth and the fact that every version still reads back.
        let conn = mem_db();
        let mut lines: Vec<String> = (0..200).map(|i| format!("line {}\n", i)).collect();
        let mut contents = vec![lines.join("")];
        let mut hashes = vec![put_object(&conn, &contents[0]).unwrap()];

        for round in 1..=60 {
            lines[50] = format!("edited round {}\n", round);
            let text = lines.join("");
            let prev = hashes.last().unwrap().clone();
            let h = put_object_against(&conn, &text, Some(&prev)).unwrap();
            contents.push(text);
            hashes.push(h);
        }

        let depth = delta_depth(&conn, hashes.last().unwrap()).unwrap();
        assert!(
            depth <= MAX_DELTA_DEPTH,
            "chain depth {} exceeds the cap {}",
            depth,
            MAX_DELTA_DEPTH
        );

        for (i, h) in hashes.iter().enumerate() {
            assert_eq!(
                get_object(&conn, h).unwrap(),
                contents[i],
                "version {} broke",
                i
            );
        }
    }
    #[test]
    fn a_sidecar_object_whose_file_is_gone_is_an_error_not_an_empty_object() {
        // #95: a sidecar object's row holds `content = x''`, `compressed = 0`, `sidecar = 1`. When
        // the file was missing, `sidecar::read` returned `None` and the read fell through to the row,
        // which answered "empty object". A grouped object whose group is missing already fails
        // closed, because `decompress_bytes(x'')` errors; this is the same case.
        let d = std::env::temp_dir().join(format!("gesta-sidecar-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();

        // Above `sidecar::INLINE_LIMIT` (4 MiB), or it stays inline and this tests nothing.
        let body: Vec<u8> = (0..(crate::store::sidecar::INLINE_LIMIT + 1024))
            .map(|i| (i % 251) as u8)
            .collect();
        let hash = put_object_bytes(&conn, &body).unwrap();
        // The object reads correctly while its file exists.
        assert_eq!(get_object_bytes(&conn, &hash).unwrap(), body);

        // Remove the sidecar file. The row stays, still saying where the body is.
        let file = crate::store::sidecar::path_for(&d, &hash).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(!file.exists());

        match get_object_bytes(&conn, &hash) {
            Ok(bytes) => panic!(
                "a missing sidecar must not read as {} byte(s) of content",
                bytes.len()
            ),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("sidecar"),
                    "the error must say what is missing, got: {msg}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn a_head_move_that_cannot_rebuild_the_tree_is_rolled_back() {
        // #96: the head UPDATE and the tree rebuild were separate statements, so a failure between
        // them left `branch_tree` drifted from `head` - the head on a commit whose files are not in
        // the tree. `status` then reports committed files as new, and `fsck` does not catch it
        // because it never compares the two. The measurement: make the tree rebuild fail, then
        // assert the head did not move.
        //
        // The obstacle is a trigger on `branch_tree`, a data-level failure; a permission trick would
        // not work because the container runs as root.
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-head-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        crate::repo::tests::commit_files(&conn, "cs1", "cs1", &[("a.txt", "one\n")]);
        set_branch_head(&conn, "main", "cs1").unwrap();
        crate::repo::tests::commit_files(&conn, "cs2", "cs2", &[("b.txt", "two\n")]);
        // `commit_files` moves the head itself, so put it back: the point of this test is the move
        // from cs1 to cs2, not the commits.
        conn.execute("UPDATE branches SET head = 'cs1' WHERE name = 'main'", [])
            .unwrap();

        let head_before: String = conn
            .query_row("SELECT head FROM branches WHERE name = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            head_before, "cs1",
            "the fixture must start from a known head"
        );

        conn.execute_batch(
            "CREATE TRIGGER refuse_tree BEFORE INSERT ON branch_tree
             BEGIN SELECT RAISE(ABORT, 'the tree refuses'); END;",
        )
        .unwrap();

        let err = set_branch_head(&conn, "main", "cs2").unwrap_err();
        assert!(
            err.to_string().contains("the tree refuses"),
            "the trigger must be the reason, not something else: {err}"
        );

        let head_after: String = conn
            .query_row("SELECT head FROM branches WHERE name = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            head_after, head_before,
            "a head move whose tree rebuild failed must be undone; head is {head_after}, was \
             {head_before}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn reading_through_a_delta_cycle_reports_corruption_instead_of_dying() {
        // #94: the recursive CTE caps each query at MAX_DELTA_DEPTH links, but a longer chain is
        // finished by calling `get_object` on the far end - and that call had no depth counter. A
        // cycle in `deltas` then recursed until the stack overflowed and killed the process. The
        // existing test covers `delta_depth` only, which is why this survived.
        //
        // Run on a big-stack thread so that a regression shows up as a failure here rather than as
        // the whole test binary aborting with no message: a stack overflow cannot be caught, so the
        // only way to report it is to give it room and let the depth counter be what stops it.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let conn = mem_db();
                for (h, b) in [("a", "b"), ("b", "a")] {
                    conn.execute(
                        "INSERT INTO deltas(hash, base, payload, created_at) VALUES(?1, ?2, x'', 1)",
                        rusqlite::params![h, b],
                    )
                    .unwrap();
                }
                // `ZResult` holds a non-Send error, so the thread hands back a plain String.
                match get_object(&conn, "a") {
                    Ok(text) => Err(format!("a two-link cycle has no base, yet the read returned: {text:?}")),
                    Err(e) => Ok(e.to_string()),
                }
            })
            .unwrap();
        let out = handle.join().expect("the read must not kill the thread");
        match out {
            Ok(msg) => assert!(
                msg.contains("cycle") || msg.contains("does not terminate"),
                "the error must name the cycle, got: {msg}"
            ),
            Err(bad) => panic!("{bad}"),
        }
    }
    #[test]
    fn a_bomb_is_refused_instead_of_allocated() {
        // #101, #156: `read_to_end` grows until the decoder says it is done, so a small compressed
        // body claiming gigabytes is allocated in full before anyone looks at it. This is not
        // hypothetical: zlib reaches about 1032:1, and the pack reader and the bundle decoder both
        // take input from other people.
        //
        // The fixture is a real bomb at a small ceiling rather than a gigabyte of zeros, so the test
        // runs in milliseconds. What is being checked is the arithmetic and the refusal, not zlib.
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&vec![0u8; 8 * 1024 * 1024]).unwrap();
        let bomb = enc.finish().unwrap();
        assert!(
            bomb.len() < 64 * 1024,
            "the fixture must be small, or it is not a bomb: {} bytes",
            bomb.len()
        );

        let err = match inflate_capped(ZlibDecoder::new(&bomb[..]), 1024 * 1024) {
            Ok(v) => panic!(
                "a {}-byte body inflated to {} bytes past a 1 MiB ceiling without complaint",
                bomb.len(),
                v.len()
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("decompression bomb") || err.contains("inflates past"),
            "the refusal must say what it thinks happened, got: {err}"
        );

        // And a body under the ceiling still inflates, or the cap would just be a bug.
        let small = compress_bytes(b"under the ceiling").unwrap();
        assert_eq!(
            decompress_bytes(&small).unwrap(),
            b"under the ceiling".to_vec()
        );
    }
    #[test]
    fn gc_leaves_no_orphan_delta_row_and_the_content_stays_readable() {
        // #276: gc deleted the `objects` row and kept the `deltas` row. `get_object_at_depth` treats a
        // `deltas` row as decisive, so when the same content was stored again later - whole, because
        // its base had been collected - every read found the stale delta first and failed the hash
        // check. The bytes were fine; the leftover row made them unreadable.
        let conn = mem_db();
        let base: String = (0..300).map(|n| format!("line {n:06}\n")).collect();
        let tip = format!("{base}one extra line\n");

        let h_base = put_object(&conn, &base).unwrap();
        let h_tip = put_object_against(&conn, &tip, Some(&h_base)).unwrap();
        assert!(
            delta_depth(&conn, &h_tip).unwrap() >= 1,
            "the fixture must store the tip as a delta, or there is nothing to orphan"
        );

        // Nothing references either object, so gc collects both.
        let deleted = gc(&conn).unwrap();
        assert!(deleted >= 2, "both objects were unreachable, got {deleted}");

        let orphan: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM deltas WHERE hash = ?1",
                params![&h_tip],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            orphan, 0,
            "gc deleted the object but left its deltas row, which makes the content unreadable later"
        );

        // The content comes back, stored whole this time because its base is gone.
        let again = put_object(&conn, &tip).unwrap();
        assert_eq!(again, h_tip, "the same bytes hash to the same name");
        let read = get_object(&conn, &again)
            .expect("content stored after a gc must be readable, not shadowed by a stale delta");
        assert_eq!(read, tip);
    }
    #[test]
    fn adding_a_column_clears_the_column_cache_it_just_invalidated() {
        // #308: `invalidate_columns` declared its own `thread_local` of a different type, so its
        // `remove` hit a map nobody wrote and the real cache kept the stale list. `ensure_column`
        // then saw the column as missing, ran the `ALTER` again, and swallowed "duplicate column"
        // as success - which hid the fact that the cache was never cleared.
        //
        // A stale entry is visible from the outside as work repeated: the second `ensure_column`
        // returns false (the column already existed) but only if the cache was refreshed. If it
        // returns true the cache lied, because it re-added a column that was already there.
        let conn = mem_db();
        conn.execute_batch("CREATE TABLE t_fresh (a TEXT)").unwrap();

        assert!(
            ensure_column(&conn, "t_fresh", "b", "TEXT").unwrap(),
            "the first call adds the column"
        );
        assert!(
            !ensure_column(&conn, "t_fresh", "b", "TEXT").unwrap(),
            "the second call must see the column the first one added, which means the cache was \
             cleared; returning true here means it re-ran the ALTER against a stale list"
        );

        // And what the cache answers now must match the table.
        let cached = columns_cached(&conn, "t_fresh").unwrap();
        assert!(cached.contains(&"b".to_string()), "cached list: {cached:?}");
    }

    #[test]
    fn clearing_a_branch_head_leaves_no_tree_behind() {
        // #309: the two statements ran with no savepoint. A crash between them left `head` NULL with
        // the old tree in place, and the next commit applied onto that stale tree - resurrecting
        // files the tree had lost.
        let conn = mem_db();
        // `mem_db` already carries a `main` branch from `init_schema`.
        conn.execute(
            "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        // Build the tree the way a commit does: one path, one object, one branch_tree row.
        let hash = put_object(&conn, "a\n").unwrap();
        conn.execute(
            "INSERT INTO branch_tree(branch, path, hash) VALUES('main', 'a.txt', ?1)",
            params![hash],
        )
        .unwrap();
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branch_tree WHERE branch = 'main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 1, "the tree starts with one path");

        clear_branch_head(&conn, "main").unwrap();

        let head: Option<String> = conn
            .query_row("SELECT head FROM branches WHERE name = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(head.is_none(), "the head is cleared");
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branch_tree WHERE branch = 'main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after, 0,
            "and the tree goes with it, not one without the other"
        );
    }
    #[test]
    fn both_readers_agree_that_a_missing_sidecar_is_corruption() {
        // #277: `get_object_bytes` was fixed under #95 to fail closed on a missing sidecar, but
        // `read_whole` still selected only `content, compressed`. It never saw the `sidecar` column,
        // so the row's empty body came back as `Ok("")` - the object read as an empty file while the
        // bytes reader called the store damaged. Two readers, one object, two answers.
        //
        // Both readers are asserted here, in one test, because the defect is the disagreement. A
        // future edit that fixes only one of them fails on the other side.
        let d = std::env::temp_dir().join(format!("gesta-sidecar-agree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();

        // Above `sidecar::INLINE_LIMIT` (4 MiB), or the body stays inline and this tests nothing.
        let body: Vec<u8> = (0..(crate::store::sidecar::INLINE_LIMIT + 1024))
            .map(|i| (i % 251) as u8)
            .collect();
        let hash = put_object_bytes(&conn, &body).unwrap();
        assert!(
            object_is_sidecar(&conn, &hash),
            "the fixture must be a sidecar object"
        );

        // Remove the file the row points at. The row stays, still claiming the body lives there.
        let file = crate::store::sidecar::path_for(&d, &hash).unwrap();
        std::fs::remove_file(&file).unwrap();

        let bytes_err = match get_object_bytes(&conn, &hash) {
            Ok(b) => panic!(
                "bytes reader returned {} byte(s) for a lost object",
                b.len()
            ),
            Err(e) => e.to_string(),
        };
        let text_err = match read_whole(&conn, &hash) {
            // The empty string is the answer this test exists to prevent: it is what made `show`
            // read a missing 4 MiB object as an empty file.
            Ok(t) => panic!(
                "text reader returned {:?} for a lost object instead of reporting corruption",
                t
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            bytes_err.contains("sidecar") && text_err.contains("sidecar"),
            "both readers must name the sidecar, got bytes={bytes_err:?} text={text_err:?}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}
