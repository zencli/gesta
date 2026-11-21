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
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> ZResult<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let existing: Vec<String> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
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
            sidecar    INTEGER NOT NULL DEFAULT 0
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
            git_commit TEXT
        );
        CREATE TABLE IF NOT EXISTS changeset_files (
            changeset_id TEXT NOT NULL,
            path         TEXT NOT NULL,
            old_hash     TEXT,
            new_hash     TEXT,
            PRIMARY KEY (changeset_id, path)
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
        "#,
    )?;

    // Columns added after the first release. A repository created by an older
    // build has the table but not the column, so the schema step upgrades it in
    // place instead of asking the operator to run a migration.
    ensure_column(conn, "remotes", "pin", "TEXT")?;
    Ok(())
}

/// Upgrade a database created before v0.3 (uncompressed TEXT objects, no
/// operation log, no tags, no remotes). Returns true when it migrated.
pub fn migrate(conn: &Connection) -> ZResult<bool> {
    let obj_cols = table_columns(conn, "objects")?;
    if obj_cols.is_empty() {
        return Ok(false); // fresh database; init_schema creates the full shape
    }

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

    // The group columns are optional: an object that has none keeps its body in `content` and reads
    // exactly as before, so adding them empty is the whole migration and no data moves.
    // A repository created before the sidecar existed needs the flag; adding it empty claims no
    // object is outside, which is exactly right for every object already stored inline.
    ensure_column(conn, "objects", "sidecar", "INTEGER NOT NULL DEFAULT 0")?;
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
    ensure_column(conn, "changesets", "git_commit", "TEXT")?;
    // The timezone a git commit was written in. Without it an export cannot reproduce the commit id,
    // because git hashes the offset as part of the author line: a commit written at +0700 and the
    // same commit written at +0000 have different names. NULL means the offset is unknown, which is
    // every changeset created here rather than imported, and those are written as +0000.
    ensure_column(conn, "changesets", "tz", "TEXT")?;
    // A signature over the changeset's identity, and the public key it verifies against.
    ensure_column(conn, "changesets", "signature", "TEXT")?;
    ensure_column(conn, "changesets", "signer", "TEXT")?;
    if !table_columns(conn, "changesets")?
        .iter()
        .any(|c| c == "parent2")
    {
        conn.execute_batch("ALTER TABLE changesets ADD COLUMN parent2 TEXT")?;
    }
    // A branch is unprotected by default, which is what every repository created before this is.
    ensure_column(conn, "branches", "protected", "INTEGER NOT NULL DEFAULT 0")?;
    init_schema(conn)?;
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

pub fn hash_content(s: &str) -> String {
    hash_bytes(s.as_bytes())
}

/// Store raw bytes as an object. No compression, no delta.
/// Binary files use this: compression often inflates them and deltas are
/// not meaningful for non-text content.
pub fn put_object_bytes(conn: &Connection, bytes: &[u8]) -> ZResult<String> {
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
    if crate::store::sidecar::is_large(bytes.len()) {
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

pub fn decompress_bytes(blob: &[u8]) -> ZResult<Vec<u8>> {
    let mut dec = ZlibDecoder::new(blob);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

fn decompress(blob: &[u8]) -> ZResult<String> {
    let mut dec = ZlibDecoder::new(blob);
    let mut out = String::new();
    dec.read_to_string(&mut out)?;
    Ok(out)
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
    let mut depth = 0usize;
    let mut cur = hash.to_string();
    // A corrupt store could contain a cycle. Stopping at the cap keeps this a
    // bounded walk rather than an infinite one.
    while depth <= MAX_DELTA_DEPTH {
        let base: Option<String> = conn
            .query_row(
                "SELECT base FROM deltas WHERE hash = ?1",
                params![cur],
                |r| r.get(0),
            )
            .optional()?;
        match base {
            Some(b) => {
                depth += 1;
                cur = b;
            }
            None => return Ok(depth),
        }
    }
    Ok(depth)
}

/// Delta chains are capped so that reading the newest version costs the same
/// whether a file has ten edits or ten thousand. git has the same problem and
/// the same shape of answer (delta chain limits plus periodic repacking).
const MAX_DELTA_DEPTH: usize = 16;

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

    // Try a delta only when a base is offered and it is a different object.
    if let Some(base_hash) = base {
        if base_hash != h {
            // A base that is already at the cap becomes a whole object instead,
            // which restarts the chain. Without this the chain grows with the
            // number of edits and every read gets slower.
            let at_cap = delta_depth(conn, base_hash)? >= MAX_DELTA_DEPTH;
            let base_text = if at_cap {
                None
            } else {
                get_object(conn, base_hash).ok()
            };
            if let Some(base_text) = base_text {
                if let Some(ops) = crate::store::delta::encode(&base_text, content) {
                    let payload = crate::store::delta::to_bytes(&ops);
                    let packed_delta = compress_bytes(&payload)?;
                    // A delta is only worth it when it beats the compressed
                    // whole object by a real margin, not by a few bytes.
                    if packed_delta.len() + 32 < whole_size {
                        conn.execute(
                            "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at)
                             VALUES(?1, X'', ?2, 0, ?3)",
                            params![h, content.len() as i64, now()],
                        )?;
                        conn.execute(
                            "INSERT OR REPLACE INTO deltas(hash, base, payload, created_at)
                             VALUES(?1, ?2, ?3, ?4)",
                            params![h, base_hash, packed_delta, now()],
                        )?;
                        return Ok(h);
                    }
                }
            }
        }
    }

    conn.execute(
        "INSERT OR IGNORE INTO objects(hash, content, size, compressed, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![h, blob, content.len() as i64, compressed, now()],
    )?;
    Ok(h)
}

/// Number of objects stored as deltas, and the bytes they save.
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

pub fn get_object(conn: &Connection, hash: &str) -> ZResult<String> {
    // A stored object is either whole, or a delta against another object. The
    // base may itself be a delta, so this recurses through get_object rather
    // than reading the base's row directly: reading the row of a delta base
    // returns an empty body, which is exactly how a nested delta silently
    // stopped applying.
    let payload: Option<Vec<u8>> = conn
        .query_row(
            "SELECT payload FROM deltas WHERE hash = ?1",
            params![hash],
            |r| r.get(0),
        )
        .optional()?;

    let Some(packed) = payload else {
        return read_whole(conn, hash);
    };

    let base_hash: String = conn.query_row(
        "SELECT base FROM deltas WHERE hash = ?1",
        params![hash],
        |r| r.get(0),
    )?;
    let base = get_object(conn, &base_hash)?;

    let raw = decompress_bytes(&packed)?;
    let ops = crate::store::delta::from_bytes(&raw)
        .ok_or_else(|| bad(format!("delta for {} is malformed", hash)))?;
    let rebuilt = crate::store::delta::apply(&base, &ops).ok_or_else(|| {
        bad(format!(
            "delta for {} does not apply to its base {}",
            hash, base_hash
        ))
    })?;

    // The hash is the contract. A rebuild that does not match means the store is
    // corrupt, and returning the wrong bytes would hide it.
    if hash_content(&rebuilt) != hash {
        // The hash is the contract, so a rebuild that disagrees means the store holds bytes it
        // cannot vouch for. That is corruption, not a missing name, and the caller should stop
        // and run fsck rather than retry.
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
    let row: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT content, compressed FROM objects WHERE hash = ?1",
            params![hash],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let (blob, compressed) = row.ok_or_else(|| {
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
    let row: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT content, compressed FROM objects WHERE hash = ?1",
            params![hash],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let (blob, compressed) = row.ok_or_else(|| {
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
    Ok(get_state(conn, "current_branch")?.unwrap_or_else(|| "main".to_string()))
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
pub fn ancestors(conn: &Connection, id: &str) -> ZResult<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        out.push(cur.clone());
        let (p1, p2) = parents_of(conn, &cur)?;
        if let Some(p) = p1 {
            stack.push(p);
        }
        if let Some(p) = p2 {
            stack.push(p);
        }
    }
    Ok(out)
}

/// Best common ancestor of two changesets: the common ancestor that every other
/// common ancestor also reaches. This is what a three-way merge diffs against.
pub fn merge_base(conn: &Connection, a: &str, b: &str) -> ZResult<Option<String>> {
    let anc_a: HashSet<String> = ancestors(conn, a)?.into_iter().collect();
    let common: Vec<String> = ancestors(conn, b)?
        .into_iter()
        .filter(|x| anc_a.contains(x))
        .collect();
    if common.is_empty() {
        return Ok(None);
    }
    if common.len() == 1 {
        return Ok(Some(common[0].clone()));
    }

    let cache: std::collections::HashMap<String, HashSet<String>> = common
        .iter()
        .map(|c| Ok((c.clone(), ancestors(conn, c)?.into_iter().collect())))
        .collect::<ZResult<_>>()?;

    // A merge base is a common ancestor that no other common ancestor reaches.
    let mut best: Option<(String, i64)> = None;
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
        if best.as_ref().map(|(_, t)| ts > *t).unwrap_or(true) {
            best = Some((cand.clone(), ts));
        }
    }
    Ok(best.map(|(id, _)| id))
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
                "SELECT id FROM changesets WHERE id LIKE ?1 ORDER BY created_at DESC LIMIT 8",
            )?;
            let rows = stmt.query_map(params![format!("{}%", rev)], |r| r.get::<_, String>(0))?;
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
    conn.execute(
        "UPDATE branches SET head = NULL WHERE name = ?1",
        params![branch],
    )?;
    conn.execute("DELETE FROM branch_tree WHERE branch = ?1", params![branch])?;
    Ok(())
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

/// How many objects the store holds. Used by `gc` to report what it did.
#[cfg(test)]
pub fn object_count(conn: &Connection) -> ZResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))?)
}

/// Collect every object reachable from the repository state, and delete the rest.
///
/// An object is reachable when it is the content of a file in a committed tree, or the body
/// of a changeset, or the target of a branch head. A packfile is not considered because the
/// database only stores loose objects; a `git gc` on the export will handle packfiles.
pub fn gc(conn: &Connection) -> ZResult<u64> {
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

    // Delete every object not in the reachable set.
    let all: Vec<String> = {
        let mut stmt = conn.prepare("SELECT hash FROM objects")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let mut deleted = 0u64;
    for hash in all {
        if !reachable.contains(&hash) {
            conn.execute(
                "DELETE FROM objects WHERE hash = ?1",
                rusqlite::params![&hash],
            )?;
            deleted += 1;
        }
    }

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
pub fn tree_at(conn: &Connection, changeset: &str) -> ZResult<Vec<(String, String)>> {
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
        "SELECT cf.changeset_id, cf.path, cf.new_hash FROM changeset_files cf
         WHERE cf.changeset_id IN ({})",
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
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Nearest to the head wins. A null hash is a deletion, which also wins over
    // anything older for that path.
    let mut seen: std::collections::HashMap<String, (usize, Option<String>)> =
        std::collections::HashMap::new();
    for (cs, path, hash) in rows {
        let d = match rank.get(cs.as_str()) {
            Some(d) => *d,
            None => continue,
        };
        match seen.get(&path) {
            Some((best, _)) if *best <= d => {}
            _ => {
                seen.insert(path, (d, hash));
            }
        }
    }

    let mut files: Vec<(String, String)> = seen
        .into_iter()
        .filter_map(|(p, (_, h))| h.map(|hash| (p, hash)))
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
}
