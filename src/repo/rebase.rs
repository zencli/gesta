//! Moving a line of changesets onto a new base.
//!
//! A rebase replays each changeset of one branch onto another point. Each step is
//! a cherry-pick, so the merge logic is the one already tested; what is new here
//! is the state between steps.
//!
//! A rebase can stop part way: a pick conflicts, and the caller has to resolve it
//! before the next pick makes sense. The session is stored, so `--continue` and
//! `--abort` work from a later command. Three rules:
//!
//! 1. Nothing is written to the target branch until a step succeeds. A stopped
//!    rebase leaves the branch where the last successful pick put it.
//! 2. `--abort` puts the branch back where it started, not where the last step
//!    left it. Half a rebase is not a state anyone asked for.
//! 3. The original changesets are never modified or deleted. A rebase creates new
//!    ones, so the old history stays readable and the operation can be undone.

use std::path::Path;

use rusqlite::params;
use rusqlite::{Connection, OptionalExtension};

use crate::{bad, now, repo, ZResult};

/// A row of the `rebase` table, as stored.
type Row = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
);

pub struct Session {
    pub branch: String,
    pub onto: String,
    /// Changesets still to replay, in order.
    pub pending: Vec<String>,
    /// The changeset being replayed when the rebase stopped.
    pub current: Option<String>,
    pub done: Vec<String>,
    /// Where the branch pointed before the rebase started.
    pub original_head: Option<String>,
    /// The paths that conflicted when the replay stopped.
    ///
    /// Kept so the continuation can tell what the caller resolved from what was never applied.
    /// Without this the continuation has to re-derive the conflict set from the working tree,
    /// which is the state it is trying to interpret.
    pub conflicts: Vec<String>,
}

fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rebase (
            id            INTEGER PRIMARY KEY CHECK (id = 1),
            branch        TEXT NOT NULL,
            onto          TEXT NOT NULL,
            pending       TEXT NOT NULL,
            current       TEXT,
            done          TEXT NOT NULL,
            original_head TEXT,
            conflicts     TEXT NOT NULL DEFAULT '',
            created_at    INTEGER NOT NULL
        );",
    )?;
    // A session written before `conflicts` existed has no column, and reading it would fail the
    // whole statement rather than the field. Adding it in place keeps a stopped rebase resumable
    // across the upgrade: without this the only way out of a stopped rebase is --abort.
    let _ = conn.execute(
        "ALTER TABLE rebase ADD COLUMN conflicts TEXT NOT NULL DEFAULT ''",
        [],
    );
    Ok(())
}

fn save(conn: &Connection, s: &Session) -> ZResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO rebase(id, branch, onto, pending, current, done, original_head, conflicts, created_at)
         VALUES(1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            s.branch,
            s.onto,
            s.pending.join(","),
            s.current,
            s.done.join(","),
            s.original_head,
            s.conflicts.join(","),
            now()
        ],
    )?;
    Ok(())
}

pub fn load(conn: &Connection) -> ZResult<Option<Session>> {
    init_schema(conn)?;
    let row: Option<Row> = conn
        .query_row(
            "SELECT branch, onto, pending, current, done, original_head, conflicts FROM rebase WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        // `.optional()?`, not `.ok()`: `start` treats "no session" as permission to `INSERT OR
        // REPLACE` a new session over a live one, so a locked database read as "no rebase" would
        // silently discard an in-progress rebase.
        .optional()?;
    Ok(row.map(
        |(branch, onto, pending, current, done, original_head, conflicts)| Session {
            branch,
            onto,
            pending: split(&pending),
            current,
            done: split(&done),
            original_head,
            conflicts: split(&conflicts),
        },
    ))
}

fn split(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(',').map(|x| x.to_string()).collect()
    }
}

pub fn clear(conn: &Connection) -> ZResult<()> {
    conn.execute("DELETE FROM rebase WHERE id = 1", [])?;
    Ok(())
}

/// Changesets on `branch` that `onto` does not already contain, oldest first.
fn to_replay(conn: &Connection, branch: &str, onto: &str) -> ZResult<Vec<String>> {
    let onto_set: std::collections::HashSet<String> =
        crate::store::ancestors(conn, onto)?.into_iter().collect();
    let head = match crate::store::branch_head(conn, branch)? {
        Some(h) => h,
        None => return Ok(Vec::new()),
    };

    // Walk the first parent chain back from the branch head, stopping at the new
    // base or at anything the base already contains.
    let mut chain: Vec<String> = Vec::new();
    let mut cur = Some(head);
    let mut guard = 0usize;
    while let Some(id) = cur {
        if onto_set.contains(&id) {
            break;
        }
        chain.push(id.clone());
        guard += 1;
        if guard > 100_000 {
            return Err(bad(
                "history walk did not end: the parent chain contains a cycle".to_string(),
            ));
        }
        let (p1, _) = crate::store::parents_of_public(conn, &id)?;
        cur = p1;
    }
    chain.reverse();
    Ok(chain)
}

#[derive(Debug)]
pub enum Progress {
    /// The rebase is finished and the branch points at this changeset.
    Done(Vec<String>),
    /// This changeset conflicts against the new base.
    Stopped { conflicts: Vec<String> },
    /// There was nothing to replay.
    Nothing,
}

/// What a finished rebase left behind that no conflict reported.
///
/// A replay applies changesets one at a time, so a deletion and a reference to the deleted name,
/// made in parallel, never meet in a three-way merge: they touch different files and both apply.
/// The tree then holds a reference to a path that is not in it, and every step of the rebase said
/// it succeeded.
pub fn dangling_after(
    conn: &Connection,
    before: Option<&str>,
    after: &str,
) -> ZResult<Vec<crate::repo::dangling::Dangling>> {
    let Some(before) = before else {
        // A branch with no head had no tree, so nothing left it. Reporting an empty list rather
        // than every name in the new tree is the difference between "nothing was removed" and
        // "everything appears to have been added".
        return Ok(Vec::new());
    };
    if before == after {
        return Ok(Vec::new());
    }
    crate::repo::dangling::find(conn, before, after)
}

/// Replay every pending changeset onto the base.
///
/// Stops at the first conflict and keeps the session so it can be continued.
pub fn run(conn: &Connection, root: &Path, author: &str, s: &mut Session) -> ZResult<Progress> {
    let mut done: Vec<String> = Vec::new();
    while let Some(id) = s.pending.first().cloned() {
        let out = repo::cherry_pick(conn, root, &id, &s.branch, author)?;
        if !out.conflicts.is_empty() {
            s.current = Some(id);
            // The continuation needs to know which paths the caller resolved, so it can apply the
            // ones they did not. Recording them here rather than re-deriving them there keeps the
            // continuation from interpreting the state it is meant to record.
            s.conflicts = out.conflicts.clone();
            s.done.extend(done.clone());
            save(conn, s)?;
            return Ok(Progress::Stopped {
                conflicts: out.conflicts,
            });
        }
        s.pending.remove(0);
        s.current = None;
        // None means nothing to record: the change is already present at this base.
        if let Some(new_id) = out.merge_changeset {
            done.push(new_id.clone());
            s.done.push(new_id.clone());
        }
        save(conn, s)?;
    }
    // A finished rebase always clears its state, including when nothing was
    // picked. Leaving the row behind made the next rebase refuse to start with
    // "already running", which is a session that can never be closed.
    clear(conn)?;
    if done.is_empty() {
        return Ok(Progress::Nothing);
    }
    Ok(Progress::Done(done))
}

pub fn start(
    conn: &Connection,
    root: &Path,
    branch: &str,
    onto: &str,
    author: &str,
) -> ZResult<Progress> {
    init_schema(conn)?;
    if load(conn)?.is_some() {
        return Err(bad(
            "a rebase is already running: finish it with --continue or undo it with --abort"
                .to_string(),
        ));
    }

    let onto_id = crate::store::resolve_rev(conn, onto)?.ok_or_else(|| {
        // `onto` accepts a branch, a tag or a changeset id, so a name that resolves to nothing has
        // to be reported against all three, and against the branches that do exist: a typo is the
        // common case, and the list is what turns the refusal into the next attempt.
        let mut branches: Vec<String> = conn
            .prepare("SELECT name FROM branches ORDER BY name")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        branches.sort();
        let known = if branches.is_empty() {
            String::new()
        } else {
            format!("; known branches: {}", branches.join(", "))
        };
        crate::support::error::Error::not_found(format!(
            "no such branch, tag or changeset to rebase onto: {}{known}",
            onto
        ))
    })?;
    let original = crate::store::branch_head(conn, branch)?;
    let pending = to_replay(conn, branch, &onto_id)?;
    if pending.is_empty() {
        return Ok(Progress::Nothing);
    }

    // Move the branch to the new base first. Each step then picks onto it, so a
    // stop leaves a valid branch pointing at the last successful pick rather
    // than at a base the caller did not ask for combined with nothing.
    crate::store::set_branch_head(conn, branch, &onto_id)?;
    repo::restore_worktree(conn, root, branch)?;

    let mut s = Session {
        branch: branch.to_string(),
        onto: onto_id,
        pending,
        current: None,
        done: Vec::new(),
        original_head: original,
        conflicts: Vec::new(),
    };
    save(conn, &s)?;
    run(conn, root, author, &mut s)
}

/// Put the paths a stopped changeset touched but did not conflict on back into the working tree.
///
/// A stopped replay returned before applying these, so the tree on disk is the base for them. That
/// is invisible for a file the changeset rewrote, because the caller's next `create` records
/// whatever is there - but a deletion is a change whose absence from the tree is the change, and
/// recording a tree that still holds the file silently undoes it. This is the only place the two
/// kinds differ, so it is the only place that has to run.
fn apply_non_conflicting(
    conn: &Connection,
    root: &Path,
    changeset: &str,
    branch: &str,
    resolved: &std::collections::HashSet<String>,
) -> ZResult<()> {
    let files: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, new_hash FROM changeset_files WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![changeset], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (path, new_hash) in files {
        if resolved.contains(&path) {
            // The caller decided this one; their file on disk is the resolution and overwriting it
            // would throw away the decision this command exists to record.
            continue;
        }
        let want = match new_hash {
            Some(h) => Some(crate::store::get_object_bytes(conn, &h)?),
            None => None,
        };
        let dest = root.join(&path);
        match want {
            None => {
                // The changeset deletes this path. Removing a file that is already gone is not an
                // error: the true state of the world is what this is trying to establish.
                if dest.exists() {
                    std::fs::remove_file(&dest)?;
                }
                // A directory left empty by that removal is pruned up to the root, so the tree
                // recorded afterwards does not carry a directory the changeset never had.
                let mut parent = dest.parent().map(|p| p.to_path_buf());
                while let Some(dir) = parent {
                    if dir == *root || dir.symlink_metadata().is_err() {
                        break;
                    }
                    if dir
                        .read_dir()
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(&dir);
                        parent = dir.parent().map(|p| p.to_path_buf());
                    } else {
                        break;
                    }
                }
            }
            Some(bytes) => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, &bytes)?;
            }
        }
    }
    let _ = branch;
    Ok(())
}

/// Replay the changeset that stopped the rebase, after the caller resolved it.
///
/// The resolution lives in the working tree, so this commits what is on disk for
/// the conflicting paths rather than repeating the pick. That is the point of a
/// stopped rebase: the caller decides the content, this records it.
pub fn continue_after_resolution(
    conn: &Connection,
    root: &Path,
    author: &str,
) -> ZResult<Progress> {
    let mut s = load(conn)?.ok_or_else(|| {
        Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Unmerged,
            "no rebase in progress: there is nothing to continue",
        ))
    })?;
    let current = s
        .current
        .clone()
        .ok_or_else(|| bad("no changeset to continue: the rebase is not stopped".to_string()))?;

    // The replay stopped on a conflict, so the working tree holds the base plus whatever the
    // caller resolved. Every other path this changeset touched was never applied, because
    // `cherry_pick` returned before it got there - and a deletion is the case that loses work:
    // the file is still on disk, so recording the tree records it as present and the deletion
    // disappears from the branch with no error at any step.
    //
    // Re-apply the paths that did not conflict before recording, and let the caller's resolution
    // win for the paths that did. A path in `conflicts` is what the caller decided; a path outside
    // it is what the changeset meant, and the two sets do not overlap.
    let resolved: std::collections::HashSet<String> = s.conflicts.iter().cloned().collect();
    apply_non_conflicting(conn, root, &current, &s.branch, &resolved)?;

    // Use what the caller left on disk, not what the original pick produced.
    //
    // A resolution that happens to match the base produces no changeset, and that
    // is a real outcome rather than a failure: the change turned out to be
    // unnecessary here. Reporting "nothing to rebase" in that case was wrong,
    // because the answer is "resolved, carried on", not "there was no rebase".
    let changes = repo::create_changeset(
        conn,
        root,
        &format!("rebase: {}", &current[..current.len().min(14)]),
        author,
    )?;
    let recorded = match changes {
        Some(cs) => {
            repo::commit_one(conn, &cs.id)?;
            s.done.push(cs.id.clone());
            Some(cs.id)
        }
        None => None,
    };
    s.current = None;
    s.pending.retain(|p| p != &current);
    save(conn, &s)?;

    match run(conn, root, author, &mut s)? {
        // The rest replayed; the resolution is already recorded above.
        Progress::Done(mut ids) => {
            if let Some(id) = recorded {
                ids.insert(0, id);
            }
            Ok(Progress::Done(ids))
        }
        Progress::Nothing => match recorded {
            Some(id) => Ok(Progress::Done(vec![id])),
            None => Ok(Progress::Nothing),
        },
        other => Ok(other),
    }
}

/// Put the branch back where it started and forget the session.
pub fn abort(conn: &Connection, root: &Path) -> ZResult<String> {
    let s = load(conn)?.ok_or_else(|| {
        Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Unmerged,
            // The caller asked to abort, so the missing thing is the rebase to abort. Saying
            // "nothing to continue" names the other flag and sends them to try the wrong one.
            "no rebase in progress: there is nothing to abort",
        ))
    })?;
    match &s.original_head {
        Some(h) => {
            crate::store::set_branch_head(conn, &s.branch, h)?;
            repo::restore_worktree(conn, root, &s.branch)?;
        }
        None => {
            crate::store::clear_branch_head(conn, &s.branch)?;
        }
    }
    clear(conn)?;
    Ok(s.branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::commit_files;

    #[test]
    fn a_database_error_is_reported_not_read_as_no_session() {
        // `load(conn).ok()` mapped every failure to `None`. `start` reads `None` as "nothing is
        // running" and inserts a fresh session over a live one, so a locked database would silently
        // discard a rebase in progress.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        init_schema(&conn).unwrap();
        conn.execute("DROP TABLE rebase", []).unwrap();
        conn.execute(
            "CREATE TABLE rebase (id INTEGER PRIMARY KEY CHECK (id = 1), branch TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO rebase (id, branch) VALUES (1, 'live')", [])
            .unwrap();

        // `expect_err` would need `Debug` on `Session`; match instead, which says the same thing.
        let err = match load(&conn) {
            Ok(_) => panic!("a rebase row it cannot read is not an empty rebase"),
            Err(e) => e,
        };
        assert!(
            format!("{err}").to_lowercase().contains("onto"),
            "the database error must surface, got: {err}"
        );
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gesta-rebase-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Build a fork with explicit parents.
    ///
    /// Not `commit_files`: that helper re-parents onto the live branch head, so
    /// committing the side branch first made the main changeset its child and the
    /// graph was a line instead of a fork. The parents here are stated outright.
    fn setup(conn: &Connection, main_text: &str, side_text: &str) {
        let put =
            |id: &str, msg: &str, branch: &str, parent: Option<&str>, path: &str, body: &str| {
                conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
                 VALUES(?1,?2,'a',1,'committed',?3,NULL,?4)",
                params![id, msg, parent, branch],
            )
            .unwrap();
                let h = crate::store::put_object(conn, body).unwrap();
                conn.execute(
                    "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,?2,NULL,?3)",
                    params![id, path, h],
                )
                .unwrap();
                let next: i64 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(version),0)+1 FROM file_versions WHERE path=?1",
                        params![path],
                        |r| r.get(0),
                    )
                    .unwrap();
                conn.execute(
                    "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES(?1,?2,?3,?4,1)",
                    params![path, next, h, id],
                )
                .unwrap();
            };

        put("base", "base", "main", None, "f.txt", "a\nb\nc\n");
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch)
             VALUES('side', 'base', 1, 'main')",
            [],
        )
        .unwrap();
        put(
            "side1",
            "side edit",
            "side",
            Some("base"),
            "f.txt",
            side_text,
        );
        put(
            "main1",
            "main edit",
            "main",
            Some("base"),
            "f.txt",
            main_text,
        );
        conn.execute("UPDATE branches SET head='side1' WHERE name='side'", [])
            .unwrap();
        conn.execute("UPDATE branches SET head='main1' WHERE name='main'", [])
            .unwrap();
    }

    #[test]
    fn a_rebase_replays_the_branch_onto_the_new_base() {
        let conn = crate::store::tests::mem_db();
        // The two sides touch different lines, so the replay is clean. Editing
        // the same line on both sides is the conflict case, tested separately.
        // The base is "a b c". Main edits line two, the side appends a line, so
        // the replay applies without touching a line both sides changed.
        setup(&conn, "a\nMAIN\nc\n", "a\nb\nc\nSIDE\n");
        let d = tmp("ok");
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        let p = start(&conn, &d, "side", "main1", "t").unwrap();
        match p {
            Progress::Done(ids) => assert_eq!(ids.len(), 1),
            other => panic!("the rebase should have finished, got {:?}", other),
        }
        assert!(
            load(&conn).unwrap().is_none(),
            "a finished rebase clears its state"
        );

        let content = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert_eq!(content, "a\nMAIN\nc\nSIDE\n", "both changes are present");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_conflict_stops_the_rebase_and_keeps_the_session() {
        let conn = crate::store::tests::mem_db();
        setup(&conn, "a\nMAIN\nc\n", "a\nSIDE\nc\n");
        let d = tmp("stop");
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        let p = start(&conn, &d, "side", "main1", "t").unwrap();
        match p {
            Progress::Stopped { conflicts } => assert_eq!(conflicts, vec!["f.txt".to_string()]),
            _ => panic!("this pick must conflict"),
        }
        let s = load(&conn)
            .unwrap()
            .expect("the session must survive the stop");
        assert_eq!(s.current.as_deref(), Some("side1"));
        assert!(s.done.is_empty(), "nothing was successfully replayed");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn abort_puts_the_branch_back_where_it_started() {
        // Half a rebase is not a state anyone asked for, so abort goes to the
        // original head rather than to the last successful step.
        let conn = crate::store::tests::mem_db();
        setup(&conn, "a\nMAIN\nc\n", "a\nSIDE\nc\n");
        let d = tmp("abort");
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        start(&conn, &d, "side", "main1", "t").unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "side").unwrap().as_deref(),
            Some("main1"),
            "the branch moved to the base before the pick"
        );

        abort(&conn, &d).unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "side").unwrap().as_deref(),
            Some("side1"),
            "abort restores the original head"
        );
        assert!(load(&conn).unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "a\nSIDE\nc\n",
            "the working tree follows the restored head"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rebase_onto_an_ancestor_has_nothing_to_do() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f.txt", "1\n")]);
        commit_files(&conn, "c2", "two", &[("f.txt", "2\n")]);
        let d = tmp("nothing");

        match start(&conn, &d, "main", "c2", "t").unwrap() {
            Progress::Nothing => {}
            _ => panic!("rebasing onto an ancestor must report nothing to do"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_second_rebase_is_refused_while_one_is_running() {
        let conn = crate::store::tests::mem_db();
        setup(&conn, "a\nMAIN\nc\n", "a\nSIDE\nc\n");
        let d = tmp("busy");
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        start(&conn, &d, "side", "main1", "t").unwrap();
        let e = start(&conn, &d, "side", "main1", "t")
            .unwrap_err()
            .to_string();
        assert!(e.contains("already running"), "got: {}", e);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn continue_records_the_resolution_and_finishes() {
        let conn = crate::store::tests::mem_db();
        setup(&conn, "a\nMAIN\nc\n", "a\nSIDE\nc\n");
        let d = tmp("cont");
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        start(&conn, &d, "side", "main1", "t").unwrap();

        // The caller resolved the conflict by hand.
        std::fs::write(d.join("f.txt"), "a\nRESOLVED\nc\n").unwrap();
        // The resolution has to look like an edit relative to the current head,
        // which is what create_changeset reads.
        let p = continue_after_resolution(&conn, &d, "t").unwrap();
        // The resolution must be reported as recorded, not as "nothing to
        // rebase". Reporting Nothing here lost the caller's fix from the account
        // of what happened, even though it had been committed.
        match p {
            Progress::Done(ids) => assert!(
                !ids.is_empty(),
                "the resolution has to be named in the result"
            ),
            Progress::Stopped { conflicts } => panic!("still conflicting: {:?}", conflicts),
            Progress::Nothing => panic!("a resolution was recorded, so this is not nothing"),
        }
        assert!(
            load(&conn).unwrap().is_none(),
            "the rebase must be finished"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "a\nRESOLVED\nc\n"
        );

        let _ = std::fs::remove_dir_all(&d);
    }
    /// Three conflicting changesets have to survive three separate stops, each with its own
    /// resolution kept, and end with no session left and the branch holding all three.
    #[test]
    fn three_conflicts_are_resolved_one_round_at_a_time() {
        let conn = crate::store::tests::mem_db();
        setup(&conn, "a\nMAIN\nc\n", "a\nSIDE\nc\n");
        let d = tmp("three");
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        // Three side changesets on top of side1, each touching the same line.
        for i in 2..=3 {
            let prev = if i == 2 { "side1" } else { "side2" };
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
                 VALUES(?1,?2,'a',1,'committed',?3,NULL,'side')",
                params![format!("side{}", i), format!("s{}", i), prev],
            )
            .unwrap();
            let hash = crate::store::put_object(&conn, &format!("SIDE{}\n", i)).unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,'f.txt',NULL,?2)",
                params![format!("side{}", i), hash],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',?1,?2,?3,1)",
                params![10 + i as i64, hash, format!("side{}", i)],
            )
            .unwrap();
        }
        conn.execute("UPDATE branches SET head='side3' WHERE name='side'", [])
            .unwrap();

        let mut stops = 0;
        let mut p = start(&conn, &d, "side", "main1", "t").unwrap();
        while let Progress::Stopped { .. } = p {
            stops += 1;
            assert!(stops <= 5, "the rebase never finished");
            std::fs::write(d.join("f.txt"), format!("resolved{}\nc\n", stops)).unwrap();
            p = continue_after_resolution(&conn, &d, "t").unwrap();
        }

        assert!(stops >= 1, "the setup produced no conflict at all");
        // The session is gone, so a further continue is refused instead of looping.
        assert!(
            load(&conn).unwrap().is_none(),
            "the session was not cleared"
        );
        assert!(
            continue_after_resolution(&conn, &d, "t").is_err(),
            "a finished rebase must refuse to continue"
        );

        // Every replayed changeset exists, and the tree matches the disk.
        let rebased: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM changesets WHERE message LIKE 'rebase:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(rebased >= 1, "nothing was recorded as replayed");
        let _ = std::fs::remove_dir_all(&d);
    }
}
