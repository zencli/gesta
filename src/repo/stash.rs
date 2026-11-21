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
use rusqlite::Connection;

use crate::now;
use crate::repo;
use crate::store::walk::{read_file, ReadOutcome};
use crate::store::{get_object, put_object_against};
use crate::{bad, ZResult};

/// One stash: what was saved, when, and under what message.
pub struct Stash {
    pub id: i64,
    pub message: String,
    pub created_at: i64,
    /// `(path, hash)`, with no hash meaning the path was deleted.
    pub files: Vec<(String, Option<String>)>,
}

fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS stashes (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            message    TEXT NOT NULL,
            author     TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS stash_files (
            stash_id INTEGER NOT NULL,
            path     TEXT NOT NULL,
            -- No hash means the path was deleted in the working tree, which is a change worth
            -- keeping: without it a stash of a deletion reported nothing to save.
            hash     TEXT,
            PRIMARY KEY (stash_id, path)
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

/// Save part of the working tree aside.
///
/// This is the non-interactive form of `git stash --patch`: the caller names the paths it wants set
/// aside and everything else stays where it is. An interactive picker is the wrong shape for an agent,
/// which knows which files it is working on and cannot answer a prompt.
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
            "INSERT INTO stash_files(stash_id, path, hash) VALUES(?1, ?2, ?3)",
            params![stash_id, rel, hash],
        )?;
        saved += 1;
    }

    // A deletion is recorded as an absence of content, which is what tells `apply` to remove
    // the path rather than write it.
    for rel in &deleted {
        tx.execute(
            "INSERT INTO stash_files(stash_id, path, hash) VALUES(?1, ?2, NULL)",
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
            .ok(),
        None => conn
            .query_row(
                "SELECT id, message, created_at FROM stashes ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok(),
    };
    let (id, message, created_at) = row.ok_or_else(|| bad("no stash to apply".to_string()))?;

    let files = {
        let mut stmt =
            conn.prepare("SELECT path, hash FROM stash_files WHERE stash_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
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

    let mut blocked: Vec<String> = stash
        .files
        .iter()
        .map(|(p, _)| p.clone())
        .filter(|p| dirty.contains(p))
        .collect();
    blocked.sort();
    if !blocked.is_empty() {
        return Ok((stash, 0, blocked));
    }

    let mut written = 0usize;
    for (rel, hash) in &stash.files {
        let target = root.join(rel);
        match hash {
            Some(h) => {
                let content = get_object(conn, h)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, content.as_bytes())?;
            }
            // No hash recorded means the change being stashed was a deletion, so applying it
            // removes the path again. Restoring the file instead would silently undo the
            // deletion the user asked to keep.
            None => {
                if target.is_file() {
                    std::fs::remove_file(&target)?;
                }
            }
        }
        written += 1;
    }

    drop_stash(conn, stash.id)?;
    Ok((stash, written, Vec::new()))
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
}
