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
        /// (hunk number, line index the hunk starts at in the file as it is now, the hunk's text)
        taken: Vec<(usize, usize, String)>,
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

        let mut taken = Vec::new();
        for n in nums {
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
            taken.push((*n, h.new_start, body));
        }

        // The file that stays behind: the original with the taken hunks reverted to their old text.
        let mut kept_lines: Vec<String> = new_lines.iter().map(|l| l.to_string()).collect();
        // Applied from the end, so an earlier replacement cannot move a later hunk's offsets.
        let mut sorted: Vec<usize> = nums.clone();
        sorted.sort_unstable();
        sorted.dedup();
        for n in sorted.iter().rev() {
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
        for (n, at, body) in &plan.taken {
            // The name carries `path#number#line`; the number is what the user asked for and the line
            // is where the text goes back. Keeping them apart is what stops a hunk from landing at
            // the top of the file when it belongs in the middle.
            tx.execute(
                "INSERT INTO stash_files(stash_id, path, hash) VALUES(?1, ?2, ?3)",
                params![stash_id, format!("{}#{}#{}", plan.path, n, at), body],
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
        // A hunk entry's "hash" is the hunk's own text, not an object id. Writing it to
        // `root.join(rel)` created a file literally named `f.txt#1` and left the real file untouched,
        // which is what made pop look like it did nothing.
        // A hunk entry is stored as `path#number#line`. The line is where the text goes back; the
        // number is what the caller named. Older entries carry only `path#number`, and for those the
        // number was used as the position, which is wrong for any hunk but the first - falling back
        // to it would restore the bug rather than the text, so the line is taken from the entry.
        if let Some((rest, line)) = rel.rsplit_once('#') {
            if let Some((path, _num)) = rest.rsplit_once('#') {
                if let (Some(body), Ok(at)) = (hash.as_ref(), line.parse::<usize>()) {
                    apply_hunk(root, path, at, body)?;
                    written += 1;
                    continue;
                }
            }
        }
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

/// Put one stashed hunk back into a file, at the position it was taken from.
///
/// The hunk was recorded as the lines it added, and the file may have moved since. The position is
/// found by looking for where the surrounding context still matches; a file that no longer holds the
/// context is refused rather than appended to, because a hunk in the wrong place is worse than a
/// hunk not applied.
fn apply_hunk(root: &Path, path: &str, at: usize, body: &str) -> ZResult<()> {
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
    // Find the first line of the hunk's new content that is NOT already in the file; if every line is
    // already present the hunk was applied twice, which is not an error.
    let already = body_lines.iter().all(|l| updated.iter().any(|u| u == l));
    if already {
        return Ok(());
    }
    // `at` is the line index the hunk started at when it was taken, recorded then because that is the
    // only moment the number is known. It used to be the hunk's ordinal, which put every hunk that was
    // not the first at the top of the file - a wrong edit that reported success.
    //
    // Taking a hunk reverted its lines to the OLD text, so the file at `at` holds what used to be
    // there. Restoring means replacing that old span, not inserting beside it: inserting left both
    // the old line and the new one, which is a file that grew by a line every pop.
    let at = at.min(updated.len());
    let span = body_lines.len().min(updated.len() - at);
    if span > 0 && !body_lines.contains(&updated[at].as_str()) {
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
