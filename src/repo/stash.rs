//! Saving work in progress without committing it.
//!
//! A stash holds the difference between the working tree and the branch head as
//! object hashes, then puts the working tree back to the head. Nothing about the
//! branch, its history or its blame changes, which is the whole point: a stash is
//! not a commit and must not look like one.
//!
//! Content goes through the same object store as a commit, so a stash costs one
//! copy of the bytes that already exist and nothing more.

use std::path::Path;

use rusqlite::params;
use rusqlite::{Connection, OptionalExtension};

use crate::now;
use crate::repo;
use crate::store::walk::{read_file, ReadOutcome};
use crate::store::{get_object, put_object_against};
use crate::{bad, ZResult};

/// What one recorded entry holds.
///
/// The entry kind used to be inferred from the key text: a whole file was `path`, and a hunk was
/// `path#number#line#replaced`. That made one string carry three unrelated things, and any file
/// whose own name had the hunk shape was read as a hunk for another path, so popping it wrote an
/// object hash into a file the stash never touched. A column says which kind it is, and the path
/// column holds only the path.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// The whole file, as the object `hash` names; no hash means the file was deleted.
    File { hash: Option<String> },
    /// One hunk, as the text it adds, and the text it displaced.
    Hunk {
        number: usize,
        at: usize,
        replaced: usize,
        body: String,
        /// The lines this hunk replaced, which is what has to still be at `at` for the hunk to go
        /// back honestly. Without it a pop inserted at a recorded index into a file that had moved,
        /// which puts the hunk on unrelated lines. The other half of a file's edits are still
        /// present after a hunk is taken, so "the file changed" cannot be the test: the file always
        /// changed. The test is whether the span this hunk came from still holds what it held.
        base: String,
    },
}

/// One stash: what was saved, when, and under what message.
pub struct Stash {
    pub id: i64,
    pub message: String,
    pub created_at: i64,
    /// `(path, entry)`, in path order.
    pub files: Vec<(String, Entry)>,
}

/// The columns `stash_files` has right now, or none when the table does not exist yet.
fn stash_file_columns(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn.prepare("PRAGMA table_info(stash_files)") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let names = stmt.query_map([], |r| r.get::<_, String>(1));
    match names {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Split a pre-column hunk key back into `(path, number, at, replaced)`.
///
/// A row written before `kind` existed carried its metadata inside `path`, so such a row is read
/// the old way. Only those rows are read this way: a row written now has `kind` set and its `path`
/// is just the path, so a tracked file named `a#1#2` stays a file.
fn parse_legacy_hunk_key(key: &str) -> Option<(&str, usize, usize, usize)> {
    let mut parts = key.rsplitn(4, '#');
    let replaced = parts.next()?.parse().ok()?;
    let at = parts.next()?.parse().ok()?;
    let number = parts.next()?.parse().ok()?;
    let path = parts.next()?;
    if path.is_empty() {
        return None;
    }
    Some((path, number, at, replaced))
}

fn init_schema(conn: &Connection) -> ZResult<()> {
    // A hunk used to be identified by a key built out of the path, `path#number#line#replaced`.
    // That put the path and three numbers in one column, so a tracked file whose own name had that
    // shape could not be told apart from a hunk entry: popping it wrote an object hash into a file
    // the stash never touched. The path column now holds only the path, and the kind and the three
    // numbers have columns of their own.
    //
    // The old primary key was `(stash_id, path)`, which cannot hold two hunks of one file once the
    // path is only the path, and SQLite cannot change a primary key in place. So a table with the
    // old shape is rebuilt once, copying its rows with their legacy keys still in `path` and
    // `kind` left NULL. `load` reads a NULL-kind row the old way, which is what keeps a stash
    // written before this change poppable.
    let cols = stash_file_columns(conn);
    if !cols.is_empty() && !cols.iter().any(|c| c == "hunk_no") {
        conn.execute_batch(
            "ALTER TABLE stash_files RENAME TO stash_files_legacy;
             CREATE TABLE stash_files (
                 stash_id INTEGER NOT NULL,
                 path     TEXT NOT NULL,
                 hash     TEXT,
                 kind     TEXT,
                 hunk_no  INTEGER NOT NULL DEFAULT 0,
                 at_line  INTEGER,
                 replaced INTEGER,
                 base     TEXT,
                 PRIMARY KEY (stash_id, path, hunk_no)
             );
             INSERT INTO stash_files(stash_id, path, hash, kind, hunk_no, at_line, replaced)
                 SELECT stash_id, path, hash, NULL, 0, NULL, NULL FROM stash_files_legacy;
             DROP TABLE stash_files_legacy;",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS stashes (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            message    TEXT NOT NULL,
            author     TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        -- `path` is the path, always. `hash` is the object for a whole file, or the text the hunk
        -- adds. `kind` is `file` or `hunk`. `hunk_no` is the hunk's ordinal, 0 for a whole file,
        -- and it is part of the key because two hunks of one file share a path.
        CREATE TABLE IF NOT EXISTS stash_files (
            stash_id INTEGER NOT NULL,
            path     TEXT NOT NULL,
            hash     TEXT,
            kind     TEXT,
            hunk_no  INTEGER NOT NULL DEFAULT 0,
            at_line  INTEGER,
            replaced INTEGER,
            base     TEXT,
            PRIMARY KEY (stash_id, path, hunk_no)
        );",
    )?;
    Ok(())
}

/// Save the working tree and reset it to the branch head.
///
/// Returns the number of files saved, or zero when there was nothing to save.
pub fn save(conn: &Connection, root: &Path, message: &str, author: &str) -> ZResult<(i64, usize)> {
    save_paths(conn, root, message, author, &[])
}

/// Save named hunks of files aside, leaving every other edit in place.
///
/// `path:number`, where the number is what `gesta diff` prints. This is the finest grain the tool
/// offers, and it exists for the case `--path` cannot express: two unrelated edits in one file, one
/// finished and one an experiment.
///
/// The content recorded in the stash is the hunk, but the file on disk keeps everything that was not
/// taken. That means the stash cannot be "the file at this moment" - it is one hunk, applied later
/// onto whatever the file has become. The hunks are stored as the text they add and remove, keyed by
/// path and position, so applying them does not need the file to be unchanged.
pub fn save_hunks(
    conn: &Connection,
    root: &Path,
    message: &str,
    author: &str,
    specs: &[String],
) -> ZResult<(i64, usize)> {
    init_schema(conn)?;
    let branch = crate::store::current_branch(conn)?;
    let committed = repo::committed_tree(conn, &branch)?;

    // Group the requested hunks by path, because the file has to be read once and rewritten once.
    let mut by_path: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for spec in specs {
        let (path, num) = spec.rsplit_once(':').ok_or_else(|| {
            bad(format!(
                "'{spec}' is not a hunk: expected path:number, as `gesta diff` prints it"
            ))
        })?;
        let n: usize = num
            .trim()
            .parse()
            .map_err(|_| bad(format!("'{num}' is not a hunk number in '{spec}'")))?;
        by_path.entry(path.to_string()).or_default().push(n);
    }

    // Read and plan every file before writing anything, so a bad number leaves the tree untouched.
    struct Plan {
        path: String,
        kept: String,
        original: String,
        /// (hunk number, where the hunk starts now, how many base lines it displaced, its text,
        /// the base text it displaced)
        ///
        /// The displaced count is recorded because it cannot be recovered later: the entry holds the
        /// text the hunk ADDS, and `apply_hunk` has no way to tell from that how many lines it
        /// replaced. Without it the restore either drained by the hunk's own length - wrong for every
        /// insertion - or drained nothing, which left the old line behind.
        taken: Vec<(usize, usize, usize, String, String)>,
    }
    let mut plans: Vec<Plan> = Vec::new();
    for (path, nums) in &by_path {
        let full = root.join(path);
        let original = match read_file(&full) {
            ReadOutcome::Text(c) => c,
            ReadOutcome::Binary => {
                return Err(bad(format!(
                    "cannot stash hunks of {path}: binary files are not tracked"
                )))
            }
            ReadOutcome::Unreadable(e) => return Err(bad(format!("cannot read {path}: {e}"))),
        };
        let old_hash = committed.get(path);
        let old_text = match old_hash {
            Some(h) => crate::store::get_object(conn, h)?,
            None => String::new(),
        };
        let hunks = crate::repo::merge::diff_hunks(&old_text, &original);
        if hunks.is_empty() {
            return Err(bad(format!(
                "{path} has no changes, so it has no hunk to take"
            )));
        }
        let old_lines: Vec<&str> = old_text.lines().collect();
        let new_lines: Vec<&str> = original.lines().collect();

        // Deduplicated before use: the primary key is `(stash_id, path)`, and each hunk is written
        // under its own row key, so naming the same hunk twice inserted the same key twice and
        // failed with a raw `UNIQUE constraint failed` after the caller had done nothing wrong.
        let mut want: Vec<usize> = nums.clone();
        want.sort_unstable();
        want.dedup();

        let mut taken = Vec::new();
        for n in &want {
            if *n == 0 || *n > hunks.len() {
                return Err(bad(format!(
                    "no hunk {n} in {path}: it has {} hunk(s), numbered from 1",
                    hunks.len()
                )));
            }
            let h = &hunks[n - 1];
            let body = new_lines
                .iter()
                .skip(h.new_start)
                .take(h.new_len)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            // Store where the hunk starts, not just its number. The number is an ordinal among the
            // hunks of this diff; putting the text back at line `number` was wrong for every hunk
            // that was not the first, and silently inserted the lines at the top of the file.
            let base = old_lines
                .iter()
                .skip(h.old_start)
                .take(h.old_len)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            taken.push((*n, h.new_start, h.old_len, body, base));
        }

        // The file that stays behind: the original with the taken hunks reverted to their old text.
        let mut kept_lines: Vec<String> = new_lines.iter().map(|l| l.to_string()).collect();
        // Applied from the end, so an earlier replacement cannot move a later hunk's offsets.
        for n in want.iter().rev() {
            let h = &hunks[n - 1];
            let replacement: Vec<String> = old_lines
                .iter()
                .skip(h.old_start)
                .take(h.old_len)
                .map(|l| l.to_string())
                .collect();
            kept_lines.splice(h.new_start..h.new_start + h.new_len, replacement);
        }
        plans.push(Plan {
            path: path.clone(),
            kept: kept_lines.join("\n") + if original.ends_with('\n') { "\n" } else { "" },
            original,
            taken,
        });
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO stashes(message, author, created_at) VALUES(?1, ?2, ?3)",
        params![message, author, now()],
    )?;
    let stash_id = tx.last_insert_rowid();

    let mut saved = 0usize;
    for plan in &plans {
        for (n, at, replaced, body, base) in &plan.taken {
            // The name carries `path#number#line#replaced`. The number is what the user asked for,
            // the line is where the text goes back, and the count is how many base lines it took out.
            // Keeping the three apart is what stops a hunk from landing at the top of the file when
            // it belongs in the middle, and what lets the old text be displaced exactly.
            //
            // Older entries carry `path#number#line` and are read as "replaced = the hunk's length",
            // which is what those entries were written under.
            // The path column holds the path and nothing else; the hunk's ordinal and its place in
            // the base text have columns of their own. Pasting them onto the path was what let a
            // file named like a key be read as a hunk.
            tx.execute(
                "INSERT INTO stash_files(stash_id, path, hash, kind, hunk_no, at_line, replaced,
                                         base)
                 VALUES(?1, ?2, ?3, 'hunk', ?4, ?5, ?6, ?7)",
                params![
                    stash_id,
                    plan.path,
                    body,
                    *n as i64,
                    *at as i64,
                    *replaced as i64,
                    base
                ],
            )?;
            saved += 1;
        }
    }
    tx.commit()?;

    // Only now does the disk change, and only for the hunks taken. Writing before the stash is durable
    // loses the work if the write fails; writing the whole file back loses the hunks not taken.
    for plan in &plans {
        let _ = plan.original;
        std::fs::write(root.join(&plan.path), &plan.kept)?;
    }

    Ok((stash_id, saved))
}

/// Save part of the working tree aside.
///
/// The caller names the paths it wants set aside and everything else stays where it is. An interactive
/// picker is the wrong shape for an agent, which knows which files it is working on and cannot answer
/// a prompt. For part of one file, `save_hunks` goes one level finer.
///
/// An empty `only` means the whole working set, which is what `save` has always meant, so the two
/// entries share one implementation rather than one of them drifting.
pub fn save_paths(
    conn: &Connection,
    root: &Path,
    message: &str,
    author: &str,
    only: &[String],
) -> ZResult<(i64, usize)> {
    init_schema(conn)?;
    let branch = crate::store::current_branch(conn)?;
    let head = crate::store::branch_head(conn, &branch)?;
    let committed = repo::committed_tree(conn, &branch)?;
    let d = repo::diff_worktree(conn, root, &branch)?;

    // Every kind of change has to be captured, including a deletion. A stash that ignored
    // deletions reported "nothing to stash" when the only change was a removed file, so
    // switching branches silently dropped the fact that a file was meant to be gone.
    let keep = |p: &String| only.is_empty() || only.iter().any(|o| o == p);
    let mut paths: Vec<String> = Vec::new();
    paths.extend(d.added.iter().filter(|p| keep(p)).cloned());
    paths.extend(d.modified.iter().filter(|p| keep(p)).cloned());
    paths.sort();
    let deleted: Vec<String> = d.deleted.iter().filter(|p| keep(p)).cloned().collect();

    if paths.is_empty() && deleted.is_empty() {
        return Ok((0, 0));
    }

    // One transaction, so a failure half way does not leave a stash pointing at
    // content that was never stored.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO stashes(message, author, created_at) VALUES(?1, ?2, ?3)",
        params![message, author, now()],
    )?;
    let stash_id = tx.last_insert_rowid();

    let mut saved = 0usize;
    for rel in &paths {
        let full = root.join(rel);
        let content = match read_file(&full) {
            ReadOutcome::Text(c) => c,
            // A binary file cannot be stashed as text. Reporting it is better
            // than silently dropping it from the stash.
            ReadOutcome::Binary => {
                return Err(bad(format!(
                    "cannot stash {}: binary files are not tracked",
                    rel
                )))
            }
            ReadOutcome::Unreadable(e) => return Err(bad(format!("cannot read {}: {}", rel, e))),
        };
        // Base the delta on whatever this path held at head, so a stash of a
        // slightly edited large file costs a delta rather than a whole copy.
        let base = committed.get(rel).map(|s| s.as_str());
        let hash = put_object_against(&tx, &content, base)?;
        tx.execute(
            "INSERT INTO stash_files(stash_id, path, hash, kind) VALUES(?1, ?2, ?3, 'file')",
            params![stash_id, rel, hash],
        )?;
        saved += 1;
    }

    // A deletion is recorded as an absence of content, which is what tells `apply` to remove
    // the path rather than write it.
    for rel in &deleted {
        tx.execute(
            "INSERT INTO stash_files(stash_id, path, hash, kind) VALUES(?1, ?2, NULL, 'file')",
            params![stash_id, rel],
        )?;
        saved += 1;
    }
    tx.commit()?;

    // Only after the stash is durable does the working tree go back to head.
    // The other order loses the work if the save fails.
    let _ = head;
    if only.is_empty() {
        repo::restore_worktree(conn, root, &branch)?;
    } else {
        // A partial stash must restore only what it took. `restore_worktree` rebuilds the whole tree
        // from the branch head, which would throw away every file the caller asked to leave alone -
        // the opposite of what a path filter means.
        for rel in paths.iter().chain(deleted.iter()) {
            match committed.get(rel) {
                Some(hash) => {
                    let body = crate::store::get_object(conn, hash)?;
                    {
                        let target = root.join(rel);
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&target, body)?;
                    }
                }
                None => {
                    // Not in the head, so it was new: the stash holds it and the tree should not.
                    let target = root.join(rel);
                    if target.is_file() {
                        std::fs::remove_file(&target)?;
                    }
                }
            }
        }
    }

    // restore_worktree only removes paths that were tracked at some point, and
    // a file added since the last commit was never tracked. Those are exactly
    // the files this stash just saved, so leaving them on disk would duplicate
    // the work: the stash holds a copy and the working tree still shows it as
    // new. Deleting them here is safe because the content is already stored.
    for rel in d.added.iter().filter(|p| keep(p)) {
        let target = root.join(rel);
        if target.is_file() {
            std::fs::remove_file(&target)?;
        }
    }

    Ok((stash_id, saved))
}

fn load(conn: &Connection, id: Option<i64>) -> ZResult<Stash> {
    init_schema(conn)?;
    let row: Option<(i64, String, i64)> = match id {
        Some(id) => conn
            .query_row(
                "SELECT id, message, created_at FROM stashes WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            // `.optional()?`, not `.ok()`: a locked or corrupt database would otherwise read as "no
            // stash to apply", and the author is told their work is gone when it is not.
            .optional()?,
        None => conn
            .query_row(
                "SELECT id, message, created_at FROM stashes ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?,
    };
    let (id, message, created_at) = row.ok_or_else(|| bad("no stash to apply".to_string()))?;

    let files = {
        let mut stmt = conn.prepare(
            "SELECT path, hash, kind, hunk_no, at_line, replaced, base
             FROM stash_files WHERE stash_id = ?1 ORDER BY path, hunk_no",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, hash, kind, hunk_no, at, replaced, base) = row?;
            let entry = match kind.as_deref() {
                Some("hunk") => Entry::Hunk {
                    number: hunk_no as usize,
                    at: at.unwrap_or(0) as usize,
                    replaced: replaced.unwrap_or(0) as usize,
                    body: hash.unwrap_or_default(),
                    base: base.unwrap_or_default(),
                },
                Some(_) => Entry::File { hash },
                // A row written before the kind column existed kept its metadata in the path. Only
                // such a row is read by its old key, so a file named `a#1#2` recorded now stays a
                // file.
                None => match parse_legacy_hunk_key(&path) {
                    Some((_real, number, at, replaced)) => Entry::Hunk {
                        number,
                        at,
                        replaced,
                        body: hash.unwrap_or_default(),
                        base: String::new(),
                    },
                    None => Entry::File { hash },
                },
            };
            // The path as recorded, and nothing else. A hunk's identity is its `Entry`, which
            // carries its own ordinal, so two hunks of one file are two rows with the same path
            // instead of one row under a key built to be unique.
            let real = match &entry {
                Entry::Hunk { .. } => parse_legacy_hunk_key(&path)
                    .map(|(real, ..)| real.to_string())
                    .unwrap_or(path.clone()),
                Entry::File { .. } => path.clone(),
            };
            out.push((real, entry));
        }
        out
    };

    Ok(Stash {
        id,
        message,
        created_at,
        files,
    })
}

pub fn list(conn: &Connection) -> ZResult<Vec<Stash>> {
    init_schema(conn)?;
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM stashes ORDER BY id DESC")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::new();
    for id in ids {
        out.push(load(conn, Some(id))?);
    }
    Ok(out)
}

/// Write a stash back onto the working tree.
///
/// A stash does not overwrite a path that the working tree has changed since, for
/// the same reason a merge refuses: the write would destroy work with no record
/// of it. The stash is kept in that case and the caller gets the list of paths
/// that blocked it.
pub fn pop(
    conn: &Connection,
    root: &Path,
    id: Option<i64>,
) -> ZResult<(Stash, usize, Vec<String>)> {
    let stash = load(conn, id)?;
    let branch = crate::store::current_branch(conn)?;
    let d = repo::diff_worktree(conn, root, &branch)?;
    let dirty: std::collections::HashSet<&String> = d
        .added
        .iter()
        .chain(d.modified.iter())
        .chain(d.deleted.iter())
        .collect();

    // A whole-file entry is blocked when its path changed, because writing over it would destroy
    // work. A hunk is not: taking one hunk leaves the file's other edits in place, so its path is
    // always dirty, and refusing on that basis refused every legitimate pop. A hunk is checked
    // where it can be checked honestly, against the span it came from, in `apply_hunk`.
    let mut blocked: Vec<String> = stash
        .files
        .iter()
        .filter(|(p, e)| matches!(e, Entry::File { .. }) && dirty.contains(p))
        .map(|(p, _)| p.clone())
        .collect();
    blocked.sort();
    if !blocked.is_empty() {
        return Ok((stash, 0, blocked));
    }

    let mut written = 0usize;
    // A hunk that cannot land is collected, not returned early: the stash stays, and the caller
    // gets every path that blocked it rather than only the first. Dropping the stash on the first
    // error would discard the work the error was about.
    let mut apply_failed: Vec<String> = Vec::new();
    for (rel, entry) in &stash.files {
        // The entry says what it is. `pop` used to decide by parsing `rel`: a key shaped
        // `path#number#line#replaced` meant a hunk, anything else meant a whole file. A tracked
        // file whose own name had that shape was therefore read as a hunk for a different path,
        // and popping wrote the hunk's text into a file the stash never touched. The kind is a
        // column now, so no path can be mistaken for anything but a path.
        match entry {
            Entry::Hunk {
                at,
                replaced,
                body,
                base,
                ..
            } => {
                if let Err(e) = apply_hunk(root, rel, *at, Some(*replaced), body, base) {
                    apply_failed.push(format!("{rel}: {e}"));
                    continue;
                }
            }
            Entry::File { hash } => {
                let target = root.join(rel);
                match hash {
                    Some(h) => {
                        let content = get_object(conn, h)?;
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&target, content.as_bytes())?;
                    }
                    // No hash recorded means the change being stashed was a deletion, so applying
                    // it removes the path again. Restoring the file instead would silently undo the
                    // deletion the user asked to keep.
                    None => {
                        if target.is_file() {
                            std::fs::remove_file(&target)?;
                        }
                    }
                }
            }
        }
        written += 1;
    }

    if !apply_failed.is_empty() {
        return Ok((stash, written, apply_failed));
    }

    drop_stash(conn, stash.id)?;
    Ok((stash, written, Vec::new()))
}

/// Put one stashed hunk back into a file, at the position it was taken from.
///
/// The hunk was recorded as the lines it added, and the file may have moved since. The position is
/// found by looking for where the surrounding context still matches; a file that no longer holds the
/// context is refused rather than appended to, because a hunk in the wrong place is worse than a
/// hunk not applied.
fn apply_hunk(
    root: &Path,
    path: &str,
    at: usize,
    replaced: Option<usize>,
    body: &str,
    base: &str,
) -> ZResult<()> {
    let full = root.join(path);
    let current = match read_file(&full) {
        ReadOutcome::Text(c) => c,
        ReadOutcome::Binary => return Err(bad(format!("cannot apply a hunk to binary {path}"))),
        ReadOutcome::Unreadable(e) => {
            return Err(bad(format!("cannot apply the hunk to {path}: {e}")))
        }
    };
    let body_lines: Vec<&str> = body.lines().collect();
    if body_lines.is_empty() {
        return Ok(());
    }
    let mut updated: Vec<String> = current.lines().map(|l| l.to_string()).collect();
    // The span this hunk came from has to still hold what it held when it was taken. This is the
    // honest test of "did the file move": the file always differs, because taking one hunk leaves
    // the other edits in place, so asking whether the file changed refuses every legitimate pop.
    // Asking whether the recorded position still holds the recorded base is a question about this
    // hunk, and it is answerable.
    let base_lines: Vec<&str> = base.lines().collect();
    if !base_lines.is_empty() {
        let here = at.min(updated.len());
        let holds = updated.len() >= here + base_lines.len()
            && updated[here..here + base_lines.len()]
                .iter()
                .zip(&base_lines)
                .all(|(have, want)| have == want);
        if !holds {
            return Err(bad(format!(
                "{path} no longer holds what this hunk replaced at line {at}, so putting it back \
                 there would land it on unrelated lines"
            )));
        }
    }
    // "Already applied" means the hunk's lines sit together at the recorded position, which is what
    // taking the hunk left behind. Asking whether every line appears anywhere in the file is a bag
    // of lines, not a position: a one line hunk adding `return nil` was skipped whenever the file
    // held `return nil` somewhere else, and `pop` then dropped the stash and reported success, so
    // the work was gone with no error.
    fn block_at(lines: &[String], at: usize, body: &[&str]) -> bool {
        lines.len() >= at + body.len()
            && lines[at..at + body.len()]
                .iter()
                .zip(body)
                .all(|(have, want)| have == want)
    }
    if block_at(&updated, at, &body_lines) {
        return Ok(());
    }
    // `at` is the line index the hunk started at when it was taken, recorded then because that is the
    // only moment the number is known. It used to be the hunk's ordinal, which put every hunk that was
    // not the first at the top of the file - a wrong edit that reported success.
    //
    // Taking a hunk reverted its lines to the OLD text, so the file at `at` holds what used to be
    // there. Restoring means replacing that old span, not inserting beside it: inserting left both
    // the old line and the new one, which is a file that grew by a line every pop.
    // How many base lines this hunk displaced is recorded at stash time, because it cannot be
    // recovered here: the entry holds the text the hunk ADDS, and no reading of that says how much
    // it replaced.
    //
    // Two earlier answers were both wrong. Draining by the hunk's own length deleted unrelated lines
    // for every insertion - base `a,b,c` plus a stashed `x,y` came back as `a,x,y`. Draining nothing
    // left the old line in place, so a replaced line stayed next to its replacement and a pop did not
    // restore the file. The count is the only honest source, and an older entry without one is read
    // as "the hunk's own length", which is what those entries were written under.
    let at = at.min(updated.len());
    let span = replaced.unwrap_or(body_lines.len()).min(updated.len() - at);
    if span > 0 {
        updated.drain(at..at + span);
    }
    for (i, line) in body_lines.iter().enumerate() {
        updated.insert(at + i, line.to_string());
    }
    let trailing = if current.ends_with('\n') || current.is_empty() {
        "\n"
    } else {
        ""
    };
    std::fs::write(&full, updated.join("\n") + trailing)?;
    Ok(())
}

pub fn drop_stash(conn: &Connection, id: i64) -> ZResult<()> {
    conn.execute("DELETE FROM stash_files WHERE stash_id = ?1", params![id])?;
    conn.execute("DELETE FROM stashes WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::commit_files;
    use crate::store::tests::mem_db;

    #[test]
    fn a_database_error_is_not_read_as_no_stash() {
        // `load(...).ok()` mapped every failure to `None`, and `None` produces "no stash to apply".
        // A locked or corrupt database therefore told the author their work was gone.
        let conn = mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        init_schema(&conn).unwrap();
        conn.execute("DROP TABLE stashes", []).unwrap();
        conn.execute(
            "CREATE TABLE stashes (id INTEGER PRIMARY KEY, message INTEGER NOT NULL, created_at INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stashes (id, message, created_at) VALUES (1, 7, 0)",
            [],
        )
        .unwrap();

        let err = match load(&conn, Some(1)) {
            Ok(_) => panic!("an unreadable message column is not an empty stash"),
            Err(e) => e,
        };
        assert!(
            format!("{err}").to_lowercase().contains("invalid"),
            "the type error must surface, got: {err}"
        );
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gesta-stash-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn repo_with_file(tag: &str) -> std::path::PathBuf {
        let d = tmp(tag);
        std::fs::write(d.join("f.txt"), "base\n").unwrap();
        d
    }

    #[test]
    fn a_path_filter_takes_only_what_it_names_and_leaves_the_rest_editable() {
        let d = tmp("pathfilter");
        let conn = mem_db();
        commit_files(
            &conn,
            "c1",
            "first",
            &[("keep.txt", "base\n"), ("take.txt", "base\n")],
        );
        std::fs::write(d.join("keep.txt"), "base\n").unwrap();
        std::fs::write(d.join("take.txt"), "base\n").unwrap();

        std::fs::write(d.join("keep.txt"), "EDITED keep\n").unwrap();
        std::fs::write(d.join("take.txt"), "EDITED take\n").unwrap();
        std::fs::write(d.join("brand-new.txt"), "never committed\n").unwrap();

        let (id, n) = save_paths(&conn, &d, "only take", "t", &["take.txt".to_string()]).unwrap();
        assert_eq!(id, 1);
        assert_eq!(n, 1, "one file, not the whole tree");
        assert_eq!(
            std::fs::read_to_string(d.join("take.txt")).unwrap(),
            "base\n",
            "the stashed path goes back to what the branch holds"
        );
        // The two assertions below are the reason this exists. `restore_worktree` rebuilds the whole
        // tree, so a filter that narrowed the save but not the reset would delete both of these.
        assert_eq!(
            std::fs::read_to_string(d.join("keep.txt")).unwrap(),
            "EDITED keep\n",
            "an unnamed path keeps its edit"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("brand-new.txt")).unwrap(),
            "never committed\n",
            "an unnamed new file is not deleted along with the stashed one"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_filter_still_means_the_whole_tree() {
        // `save` delegates here, so this is what keeps the two entry points from drifting apart.
        let d = repo_with_file("emptyfilter");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);
        std::fs::write(d.join("f.txt"), "changed\n").unwrap();
        let (_, n) = save_paths(&conn, &d, "wip", "t", &[]).unwrap();
        assert_eq!(n, 1, "an empty filter selects everything");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saving_a_stash_empties_the_working_tree() {
        let d = repo_with_file("save");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        std::fs::write(d.join("f.txt"), "changed\n").unwrap();
        std::fs::write(d.join("new.txt"), "added\n").unwrap();

        let (id, n) = save(&conn, &d, "wip", "t").unwrap();
        assert_eq!(id, 1);
        assert_eq!(n, 2, "both the changed and the new file are saved");
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "base\n");
        assert!(!d.join("new.txt").exists(), "an added file is removed too");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pure_addition_does_not_delete_the_lines_below_it() {
        // #120: the span drained was the hunk's own length, but a hunk is recorded as the lines it
        // ADDED. A pure addition has no base lines, so draining by its length removed unrelated
        // lines. Base `a,b,c` with a stashed `x,y` came back as `a,x,y`.
        let d = std::env::temp_dir().join(format!("zf_hunk_pure_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();

        apply_hunk(&d, "f.txt", 1, Some(0), "x\ny", "").unwrap();

        let got = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert_eq!(
            got, "a\nx\ny\nb\nc\n",
            "a pure addition must not remove the lines under it"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stashed_addition_at_the_end_survives_a_pop() {
        // #120 end to end: append two lines, stash, pop. The base lines must still be there.
        let d = repo_with_file("hunkappend");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "a\nb\nc\n")]);

        std::fs::write(d.join("f.txt"), "a\nb\nc\nx\ny\n").unwrap();
        save(&conn, &d, "wip", "t").unwrap();

        // Take the hunk back out, which is the state pop starts from.
        std::fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();

        let (_s, _written, blocked) = pop(&conn, &d, None).unwrap();
        assert!(blocked.is_empty(), "{blocked:?}");
        let got = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert!(
            got.contains("b") && got.contains("c"),
            "the base lines were deleted by the pop: {got:?}"
        );
        assert!(
            got.contains("x") && got.contains("y"),
            "the hunk was lost: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn popping_restores_both_the_edit_and_the_new_file() {
        let d = repo_with_file("pop");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        std::fs::write(d.join("f.txt"), "edited\n").unwrap();
        std::fs::write(d.join("new.txt"), "added\n").unwrap();
        save(&conn, &d, "wip", "t").unwrap();

        let (_s, written, blocked) = pop(&conn, &d, None).unwrap();
        assert!(blocked.is_empty());
        assert_eq!(written, 2);
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "edited\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("new.txt")).unwrap(),
            "added\n"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn popping_refuses_when_the_working_tree_has_changed() {
        // Writing over a locally changed path would destroy work that was never
        // recorded anywhere. The stash stays so the caller can try again.
        let d = repo_with_file("refuse");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        std::fs::write(d.join("f.txt"), "stashed\n").unwrap();
        save(&conn, &d, "wip", "t").unwrap();
        std::fs::write(d.join("f.txt"), "local only\n").unwrap();

        let (_s, written, blocked) = pop(&conn, &d, None).unwrap();
        assert_eq!(written, 0, "nothing is written when a path conflicts");
        assert_eq!(blocked, vec!["f.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "local only\n",
            "the local change is untouched"
        );
        assert_eq!(list(&conn).unwrap().len(), 1, "the stash is kept");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saving_with_nothing_to_save_reports_zero() {
        let d = repo_with_file("empty");
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("f.txt", "base\n")]);

        let (id, n) = save(&conn, &d, "wip", "t").unwrap();
        assert_eq!((id, n), (0, 0));
        let _ = std::fs::remove_dir_all(&d);
    }
    /// A stash that ignores deletions silently drops a change. The first version only
    /// collected added and modified paths, so a working tree whose only change was a removed
    /// file reported "nothing to stash" and the deletion was left behind when the branch
    /// moved.
    #[test]
    fn a_deletion_alone_is_still_a_change_worth_saving() {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-stash-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        crate::repo::tests::commit_files(
            &conn,
            "s1",
            "base",
            &[("keep.txt", "keep\n"), ("gone.txt", "gone\n")],
        );
        crate::store::set_branch_head(&conn, "main", "s1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();
        assert!(dir.join("gone.txt").exists());

        std::fs::remove_file(dir.join("gone.txt")).unwrap();

        let (id, saved) = save(&conn, &dir, "deletion", "a").unwrap();
        assert_eq!(saved, 1, "the deletion was not saved");
        assert!(id > 0);
        // The working tree goes back to the head, so the file is present again.
        assert!(
            dir.join("gone.txt").exists(),
            "stashing must restore the head state"
        );

        let (_stash, written, blocked) = pop(&conn, &dir, None).unwrap();
        assert!(blocked.is_empty(), "unexpected conflict: {:?}", blocked);
        assert_eq!(written, 1);
        assert!(
            !dir.join("gone.txt").exists(),
            "popping must apply the deletion again"
        );
        assert!(
            dir.join("keep.txt").exists(),
            "an unrelated file was touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_three_kinds_of_change_survive_a_stash() {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-stash-3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        crate::repo::tests::commit_files(
            &conn,
            "t1",
            "base",
            &[("a.txt", "one\n"), ("b.txt", "two\n"), ("c.txt", "three\n")],
        );
        crate::store::set_branch_head(&conn, "main", "t1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();

        std::fs::write(dir.join("a.txt"), "one MODIFIED\n").unwrap();
        std::fs::write(dir.join("d.txt"), "brand new\n").unwrap();
        std::fs::remove_file(dir.join("c.txt")).unwrap();

        let (_id, saved) = save(&conn, &dir, "all three", "a").unwrap();
        assert_eq!(saved, 3, "expected every kind of change to be saved");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(
            dir.join("c.txt").exists(),
            "the head state was not restored"
        );
        assert!(!dir.join("d.txt").exists(), "the new file was not cleared");

        pop(&conn, &dir, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one MODIFIED\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("d.txt")).unwrap(),
            "brand new\n"
        );
        assert!(
            !dir.join("c.txt").exists(),
            "the deletion was not reapplied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_named_like_a_hunk_key_is_not_read_as_a_hunk() {
        // The old hunk key was `path#number#line#replaced`, so a file whose name has three `#`
        // groups parses exactly like a hunk entry: `a#1#2#3` reads as hunk 1 of `a`, at line 2,
        // displacing 3.
        // The entry key space is shared: a whole-file entry and a hunk entry are both just text in
        // `stash_files.path`, and `pop` decides which by parsing `path#number#line`. A tracked file
        // whose own name has that shape parses as a hunk for a different path, so popping one
        // writes an object hash into a file the stash never touched.
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-stash-hashname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        commit_files(&conn, "h1", "base", &[("a#1#2#3", "base\n")]);
        crate::store::set_branch_head(&conn, "main", "h1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();

        std::fs::write(dir.join("a#1#2#3"), "edited\n").unwrap();
        save(&conn, &dir, "keep the edit", "a").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a#1#2#3")).unwrap(),
            "base\n"
        );

        pop(&conn, &dir, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a#1#2#3")).unwrap(),
            "edited\n",
            "the file is restored by its own name"
        );
        assert!(
            !dir.join("a").exists(),
            "no file was created at the path the key pretended to name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hunk_is_not_skipped_because_its_line_exists_elsewhere() {
        // "Already applied" used to mean "every line of the hunk appears somewhere in the file".
        // That is a bag of lines, not a position: a one line hunk adding `return nil` was skipped
        // whenever the file held `return nil` anywhere else, and `pop` then dropped the stash and
        // reported success, so the work was gone with no error.
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-stash-bag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        commit_files(
            &conn,
            "b1",
            "base",
            &[("f.txt", "alpha\nreturn nil\nomega\n")],
        );
        crate::store::set_branch_head(&conn, "main", "b1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();

        // Append the same line again, at the end. The file then holds `return nil` both in the
        // middle and nowhere near where the hunk belongs.
        std::fs::write(dir.join("f.txt"), "alpha\nreturn nil\nomega\nreturn nil\n").unwrap();
        let (_id, saved) = save_hunks(
            &conn,
            &dir,
            "add a redundant line",
            "t",
            &["f.txt:1".to_string()],
        )
        .unwrap();
        assert_eq!(saved, 1, "the hunk was recorded");
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "alpha\nreturn nil\nomega\n",
            "saving the hunk reverted its line"
        );

        pop(&conn, &dir, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "alpha\nreturn nil\nomega\nreturn nil\n",
            "the hunk goes back even though its line already appears elsewhere in the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn naming_the_same_hunk_twice_saves_it_once() {
        // The same hunk named twice used to insert the same primary key twice and fail with a raw
        // `UNIQUE constraint failed`, which tells the caller nothing they can act on. Naming it
        // twice is not a mistake worth an error: it is one hunk.
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-stash-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        commit_files(&conn, "d1", "base", &[("f.txt", "a\nb\n")]);
        crate::store::set_branch_head(&conn, "main", "d1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();
        std::fs::write(dir.join("f.txt"), "a\nB\n").unwrap();

        let (_id, saved) = save_hunks(
            &conn,
            &dir,
            "same hunk twice",
            "t",
            &["f.txt:1".to_string(), "f.txt:1".to_string()],
        )
        .unwrap();
        assert_eq!(saved, 1, "one hunk, however many times it is named");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hunk_pop_is_blocked_when_its_file_has_local_changes() {
        // The dirty guard compared stash keys against worktree paths. A hunk key was
        // `path#number#line`, which never equals a path, so the guard never fired for a hunk and
        // `apply_hunk` wrote at a recorded index into a file that had moved since: the hunk landed
        // on unrelated lines. The guard exists for exactly that case and was the one bypassed.
        let conn = crate::store::tests::mem_db();
        let dir =
            std::env::temp_dir().join(format!("gesta-stash-dirtyhunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        commit_files(&conn, "y1", "base", &[("f.txt", "a\nb\nc\n")]);
        crate::store::set_branch_head(&conn, "main", "y1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();

        std::fs::write(dir.join("f.txt"), "a\nb\nC\n").unwrap();
        let (_id, saved) =
            save_hunks(&conn, &dir, "one hunk", "t", &["f.txt:1".to_string()]).unwrap();
        assert_eq!(saved, 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "a\nb\nc\n",
            "taking the hunk put the old line back"
        );

        // A local edit since the stash. Popping now would write at a stale index.
        std::fs::write(dir.join("f.txt"), "NEW\na\nb\nc\n").unwrap();
        let (_s, written, blocked) = pop(&conn, &dir, None).unwrap();
        assert_eq!(written, 0, "nothing is written over a locally changed file");
        assert_eq!(blocked.len(), 1, "one path is named: {blocked:?}");
        assert!(
            blocked[0].starts_with("f.txt: "),
            "the path comes first, then why: {blocked:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "NEW\na\nb\nc\n",
            "the local edit survives"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
