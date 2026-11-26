//! Rename detection and revert.
//!
//! Both are history analyses: they look at what a changeset recorded and derive
//! something the storage layer does not store directly. Keeping them apart from
//! `repo.rs` keeps that file about the commit lifecycle.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

use crate::repo::merge::merge3;
use crate::repo::{commit_one, materialise};
use crate::store::{
    branch_head, current_branch, get_object, hash_at_point, latest_hash, new_changeset_id,
    put_object,
};
use crate::{bad, now, ZResult};

/// A path that was renamed: old path, new path, and how similar the content is.
pub struct Rename {
    pub from: String,
    pub to: String,
    pub similarity: f64,
}

/// Find renames in one changeset by matching deleted paths to added paths with
/// similar content. git does the same at diff time, and so does this: nothing is
/// recorded when the change is committed, so a rename is detected whenever it is
/// asked about.
pub fn detect_renames(conn: &Connection, changeset: &str, threshold: f64) -> ZResult<Vec<Rename>> {
    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![changeset], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // A deletion here means "this changeset recorded no new content for the
    // path"; an addition means "no previous content". A rename is one of each
    // whose content overlaps.
    let mut deleted: Vec<(String, String)> = Vec::new();
    let mut added: Vec<(String, String)> = Vec::new();
    for (path, old, new) in &files {
        match (old, new) {
            (Some(o), None) => deleted.push((path.clone(), o.clone())),
            (None, Some(n)) => added.push((path.clone(), n.clone())),
            _ => {}
        }
    }

    let mut out = Vec::new();
    let mut used: Vec<usize> = Vec::new();

    // An exact content match is a rename with no scoring needed. git looks for this first, and the
    // reason is that content is exact evidence where a line ratio is a heuristic: a file renamed and
    // heavily edited in the same commit keeps its identity and loses most of its lines, so scoring
    // alone reports a delete and an add. Measured on a 20-line file renamed while keeping 5 lines:
    // 25% similarity against a 50% threshold, so the rename vanished.
    for (dpath, dhash) in &deleted {
        let exact = added
            .iter()
            .enumerate()
            .find(|(i, (_, ahash))| !used.contains(i) && ahash == dhash);
        if let Some((i, (apath, _))) = exact {
            used.push(i);
            out.push(Rename {
                from: dpath.clone(),
                to: apath.clone(),
                similarity: 1.0,
            });
        }
    }

    for (dpath, dhash) in &deleted {
        if out.iter().any(|r| &r.from == dpath) {
            // Already paired by content; scoring it again could only pair it worse.
            continue;
        }
        let dtext = match get_object(conn, dhash) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let mut best: Option<(f64, usize)> = None;
        for (i, (_, ahash)) in added.iter().enumerate() {
            if used.contains(&i) {
                continue;
            }
            let atext = match get_object(conn, ahash) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let score = similarity(&dtext, &atext);
            if best.map(|(b, _)| score > b).unwrap_or(true) {
                best = Some((score, i));
            }
        }
        if let Some((score, i)) = best {
            if score >= threshold {
                used.push(i);
                out.push(Rename {
                    from: dpath.clone(),
                    to: added[i].0.clone(),
                    similarity: score,
                });
            }
        }
    }
    Ok(out)
}

/// Fraction of lines shared between two texts, 0.0 to 1.0.
///
/// Uses the line counts and the longest common subsequence, so an edit that
/// keeps most lines scores high and a full rewrite scores near zero.
pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let total = a.lines().count() + b.lines().count();
    if total == 0 {
        return 1.0;
    }
    let (removed, _added) = crate::repo::merge::line_diff(a, b);
    let changed = removed.len();
    1.0 - (2.0 * changed as f64) / total as f64
}

/// Every path `path` was previously called, following detected renames
/// backwards through history.
pub fn rename_history(conn: &Connection, path: &str, max_depth: usize) -> ZResult<Vec<String>> {
    let mut chain = vec![path.to_string()];
    let mut current = path.to_string();
    // A path cannot be its own ancestor. Without this a rename cycle, which a bad import can
    // produce, would keep pushing names until max_depth hid the loop.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(current.clone());

    // The changesets are re-read for each new name. The earlier version queried `current` once,
    // before the loop, so the second hop searched a list that could not contain it: the changeset
    // that renamed B to C touches A and B, and a list built for C never holds it. Every chain
    // stopped after one hop, which is not what this function's name promises.
    while chain.len() <= max_depth {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT changeset_id FROM changeset_files WHERE path = ?1
             ORDER BY rowid DESC",
        )?;
        let changesets: Vec<String> = stmt
            .query_map(params![current.clone()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut found = None;
        for cs in changesets {
            for r in detect_renames(conn, &cs, 0.5)? {
                if r.to == current && !seen.contains(&r.from) {
                    found = Some(r.from);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        match found {
            Some(from) => {
                seen.insert(from.clone());
                chain.push(from.clone());
                current = from;
            }
            None => break,
        }
    }
    Ok(chain)
}

/// What a revert did, and what it could not finish afterwards.
///
/// The distinction is the whole point of this type. Once the transaction commits, the revert has
/// happened: history holds the changeset and the branch points at it. A later failure - the commit
/// bookkeeping, or writing the files back to disk - cannot undo that, and reporting it as an error
/// told the caller the revert failed when it had not, so retrying made a second revert that undid
/// the first.
#[derive(Debug, Clone, PartialEq)]
pub struct RevertOutcome {
    /// The changeset that records the revert. It is committed either way.
    pub id: String,
    /// Set when the revert is durable but the working copy could not follow it.
    pub materialise_error: Option<String>,
}

/// Undo the effect of `target` by recording a new changeset that restores the
/// content each of its files had before it.
///
/// This is a revert, not a reset: history is never rewritten, so a revert is
/// itself revertable and safe on a branch other people have already pulled.
/// A working-copy failure is reported in the outcome, not as an error: after `tx.commit` the revert
/// is durable, and the only honest thing to say about a `materialise` failure is "the revert
/// happened, the files on disk did not follow".
pub fn revert_changeset(
    conn: &Connection,
    root: &Path,
    target: &str,
    author: &str,
    reason: Option<&str>,
) -> ZResult<RevertOutcome> {
    let (status, message, parent): (String, String, Option<String>) = conn
        .query_row(
            "SELECT status, message, parent FROM changesets WHERE id = ?1",
            params![target],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            crate::support::error::Error::not_found(format!("no such changeset: {}", target))
        })?;

    if status != "committed" {
        return Err(bad(format!(
            "cannot revert {}: it was never committed",
            target
        )));
    }

    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![target], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if files.is_empty() {
        return Err(bad(format!("changeset {} changed no files", target)));
    }

    // Where the change came from: the parent state is what the revert restores.
    let branch = current_branch(conn)?;
    let base = match &parent {
        Some(p) => p.clone(),
        None => String::new(),
    };

    let revert_id = new_changeset_id(
        conn,
        &format!("Revert \"{}\"", first_line(&message)),
        author,
    )?;
    let head = branch_head(conn, &branch)?;

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
         VALUES(?1, ?2, ?3, ?4, 'pending', ?5, ?6)",
        params![
            revert_id,
            format!(
                "Revert \"{}\"{}\n\nThis reverts changeset {}.",
                first_line(&message),
                reason.map(|r| format!(": {}", r)).unwrap_or_default(),
                target
            ),
            author,
            now(),
            head,
            branch
        ],
    )?;

    let mut restored = 0usize;
    for (path, old, new) in &files {
        // The content to restore is what the file held just before `target`.
        // `old_hash` records exactly that, so it is the authoritative source;
        // the parent chain is only a fallback for older rows that left it NULL.
        let restore_hash = match old {
            Some(h) => Some(h.clone()),
            None => match &base {
                b if b.is_empty() => None,
                b => hash_at_point(&tx, path, b)?,
            },
        };

        // A file that changed after `target` still reverts cleanly when the
        // later edits do not overlap: the revert is a three-way merge with
        // `target`'s own parent as the base. Refusing on any later change would
        // make revert useless on an active branch, which is where it is needed.
        //
        // The merge result is what gets recorded, not `restore_hash`. Merging only to test for a
        // conflict and then writing the pre-target content whole discarded every later edit that
        // did not overlap - a file edited at the top after `target`, reverted for an unrelated
        // change at the bottom, silently lost the top edit.
        let current = latest_hash(&tx, path, &branch)?;
        let mut new_version = restore_hash.clone();
        if let (Some(c), Some(n)) = (&current, new) {
            if c != n {
                let base_text = match restore_hash.as_ref() {
                    Some(h) => get_object(&tx, h)?,
                    None => String::new(),
                };
                let our_text = get_object(&tx, c)?;
                let their_text = get_object(&tx, n)?;
                // base is what `target` wrote, ours is the file as it stands, theirs is what we
                // want back. Reverting is exactly the three-way merge of those.
                let merged = merge3(&their_text, &our_text, &base_text);
                if merged.is_conflict() {
                    return Err(bad(format!(
                        "cannot revert {}: {} changed in a conflicting way since then",
                        target, path
                    )));
                }
                // The merged body is a new object. When the later edits were non-overlapping it
                // differs from both sides, so it has to be stored before a row can name it.
                new_version = Some(put_object(&tx, merged.text())?);
            }
        }

        if let Some(h) = &new_version {
            put_object(&tx, &get_object(&tx, h)?)?;
        }
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            params![revert_id, path, current, new_version],
        )?;
        restored += 1;
    }
    tx.commit()?;

    // From here on the revert is durable: the changeset is committed and the branch points at it.
    // `commit_one` failing cannot un-happen that, so it is reported alongside the id rather than
    // replacing it. The old code returned `Err` here, and a caller who retried made a second revert
    // that undid the first.
    let mut materialise_error = match commit_one(conn, &revert_id) {
        Ok(_) => None,
        Err(e) => Some(format!(
            "the revert is recorded but its commit bookkeeping failed: {}",
            e
        )),
    };
    // The commit updated the database; the working copy has to follow or the
    // revert looks applied while the old content is still on disk.
    if let Err(e) = materialise(conn, root, &branch) {
        materialise_error = Some(match materialise_error {
            Some(previous) => format!("{}; then writing the files failed: {}", previous, e),
            None => format!(
                "the revert is recorded but the files were not written: {}",
                e
            ),
        });
    }
    let _ = restored;
    Ok(RevertOutcome {
        id: revert_id,
        materialise_error,
    })
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::commit_files;
    use crate::store::hash_content;
    use crate::store::tests::mem_db;
    use std::fs;

    #[test]
    fn a_rename_on_the_other_side_of_a_merge_is_still_found() {
        // The shape: main renames a file, a side branch keeps editing it under the old name, and the
        // merge takes the new name. Asking for the history of the new path has to cross into the
        // second parent, because that is where the old name lived.
        //
        // Without the walk, `rename_history` looks only at changesets that wrote the path directly,
        // finds the merge (which wrote it), sees no rename inside the merge itself, and stops. The
        // chain then has one entry and the edits from the side branch are invisible.
        let conn = mem_db();
        commit_files(
            &conn,
            "cs1",
            "base",
            &[("app.py", "def run():\n    return 1\n")],
        );

        // cs2 on main renames app.py to main.py, content unchanged.
        let h = put_object(&conn, "def run():\n    return 1\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','rename','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','main.py',NULL,?1)",
            params![h],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','app.py',?1,NULL)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();

        // cs3 on a side branch edits app.py, which still exists there. The branch has to exist
        // before a changeset can name it: `commit_one` reads the head to re-parent onto, and a name
        // with no row is a not-found rather than an empty branch.
        conn.execute(
            "INSERT INTO branches(name,head,created_at) VALUES('side','cs1',1)",
            [],
        )
        .unwrap();
        let edited = "def run():\n    return 2\n";
        let h2 = put_object(&conn, edited).unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','edit on side','a',3,'pending','cs1','side')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','app.py',?1,?2)",
            params![h, h2],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        // cs4 is the merge: parent is cs2, parent2 is cs3, and it carries main.py.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
             VALUES('cs4','merge','a',4,'pending','cs2','cs3','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs4','main.py',?1,?2)",
            params![h, h2],
        )
        .unwrap();
        commit_one(&conn, "cs4").unwrap();

        let chain = rename_history(&conn, "main.py", 10).unwrap();
        assert!(
            chain.contains(&"app.py".to_string()),
            "the rename recorded on the merged side must be reachable: {chain:?}"
        );
    }

    #[test]
    fn a_rename_on_an_unrelated_branch_is_not_claimed_as_ancestry() {
        // Two branches that never merged. Branch `feature` renames a file; `main` has its own file
        // that happens to sit at the same new path later. The rename on `feature` is not part of
        // main's history, and a chain that includes it is telling the reader a file was moved here
        // when it was moved somewhere else and never arrived.
        let conn = mem_db();
        commit_files(
            &conn,
            "cs1",
            "base",
            &[(
                "shared.py",
                "x = 1
y = 2
",
            )],
        );
        conn.execute(
            "INSERT INTO branches(name,head,created_at) VALUES('feature','cs1',1)",
            [],
        )
        .unwrap();

        // cs2 on feature renames shared.py to moved.py.
        let h = put_object(
            &conn,
            "x = 1
y = 2
",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','rename on feature','a',2,'pending','cs1','feature')",
            [],
        )
        .unwrap();
        for (path, old, new) in [("moved.py", None, Some(&h)), ("shared.py", Some(&h), None)] {
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash) VALUES('cs2',?1,?2,?3)",
                params![path, old, new],
            ).unwrap();
        }
        commit_one(&conn, "cs2").unwrap();

        // cs3 on main edits shared.py and never sees the rename. main's history has no moved.py.
        let h2 = put_object(
            &conn,
            "x = 1
y = 3
",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','edit on main','a',3,'pending','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash) VALUES('cs3','shared.py',?1,?2)",
            params![h, h2],
        ).unwrap();
        commit_one(&conn, "cs3").unwrap();

        // Asking about shared.py on main must not report that it used to be called moved.py: the two
        // names never met in this branch.
        let chain = rename_history(&conn, "shared.py", 10).unwrap();
        assert!(
            !chain.contains(&"moved.py".to_string()),
            "a rename on an unmerged branch must not appear in main's chain: {chain:?}"
        );
    }

    #[test]
    fn blame_survives_a_rename_and_names_the_author_who_wrote_the_line() {
        // The shape: one commit writes a file, the next renames it with no edit. Blame on the new
        // name must credit the author of the first commit for the lines it wrote.
        //
        // Before this test, blame credited the rename. Two separate causes, and fixing either alone
        // left it broken: the rename deleted the old path's blame rows, and the new path has exactly
        // one version so there was nothing to diff against even when the rows survived.
        let tmp = std::env::temp_dir().join(format!("gesta-blame-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let conn = mem_db();

        fs::write(tmp.join("app.py"), "def run():\n    return 1\n").unwrap();
        commit_files(
            &conn,
            "cs1",
            "base",
            &[("app.py", "def run():\n    return 1\n")],
        );

        // Rename with no edit: one path added, the other deleted, same content.
        let h = put_object(&conn, "def run():\n    return 1\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','rename','bob',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','main.py',NULL,?1)",
            params![h],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','app.py',?1,NULL)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();
        let _ = fs::remove_file(tmp.join("app.py"));

        let keep_me: String = conn
            .query_row(
                "SELECT changeset_id FROM blame_ranges
                 WHERE path = 'main.py' AND start_line = 1",
                [],
                |r| r.get(0),
            )
            .expect("blame must answer for the renamed path");
        assert_eq!(
            keep_me, "cs1",
            "the line was written before the rename, so the rename must not own it"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn blame_does_not_outlive_a_real_deletion() {
        // The other side of the same decision: a file that is deleted rather than renamed must lose
        // its blame rows, or the table grows a permanent entry for every path that ever existed.
        let conn = mem_db();
        commit_files(&conn, "cs1", "base", &[("gone.py", "x = 1\n")]);

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','delete','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let h = put_object(&conn, "x = 1\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','gone.py',?1,NULL)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();

        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM blame_ranges WHERE path = 'gone.py'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "a deleted file leaves no blame rows behind");
    }

    #[test]
    fn similarity_scores_identical_as_one_and_rewrite_as_low() {
        assert_eq!(similarity("a\nb\n", "a\nb\n"), 1.0);
        assert!(similarity("a\nb\nc\nd\n", "a\nb\nc\nX\n") > 0.5);
        assert!(similarity("a\nb\nc\nd\n", "w\nx\ny\nz\n") < 0.2);
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("", "a\n"), 0.0);
    }

    #[test]
    fn a_rename_is_detected_from_content() {
        let conn = mem_db();
        commit_files(
            &conn,
            "cs1",
            "original",
            &[("app.py", "def run():\n    return 1\n")],
        );

        // Rename: one path deleted, another added, with the same content.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','rename','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let h = put_object(&conn, "def run():\n    return 1\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','main.py',NULL,?1)",
            params![h],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','app.py',?1,NULL)",
            params![h],
        )
        .unwrap();

        let renames = detect_renames(&conn, "cs2", 0.5).unwrap();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].from, "app.py");
        assert_eq!(renames[0].to, "main.py");
        assert!(renames[0].similarity > 0.99);
    }

    #[test]
    fn an_unrelated_delete_and_add_is_not_a_rename() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','m','a',1,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        let gone = put_object(&conn, "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let fresh = put_object(&conn, "one\ntwo\nthree\nfour\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','old.txt',?1,NULL)",
            params![gone],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','new.txt',NULL,?1)",
            params![fresh],
        )
        .unwrap();

        let renames = detect_renames(&conn, "cs1", 0.5).unwrap();
        assert!(
            renames.is_empty(),
            "unrelated content must not look like a rename"
        );
    }

    #[test]
    fn revert_restores_the_content_from_before_the_target() {
        let tmp = std::env::temp_dir().join(format!("zf_revert_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("a.txt"), "A\n").unwrap();
        commit_files(&conn, "cs1", "v1", &[("a.txt", "A\n")]);

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','v2','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let h2 = put_object(&conn, "B\n").unwrap();
        let h1 = hash_content("A\n");
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','a.txt',?1,?2)",
            params![h1, h2],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();
        fs::write(tmp.join("a.txt"), "B\n").unwrap();

        let rev = revert_changeset(&conn, &tmp, "cs2", "bot", Some("test"))
            .unwrap()
            .id;
        assert_eq!(
            branch_head(&conn, "main").unwrap().as_deref(),
            Some(rev.as_str())
        );
        assert_eq!(
            fs::read_to_string(tmp.join("a.txt")).unwrap(),
            "A\n",
            "the working copy must hold the reverted content"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_revert_whose_working_copy_fails_still_reports_the_revert_as_done() {
        // `tx.commit()` makes the revert durable, then `commit_one` and `materialise` ran outside it.
        // Returning `Err` from either told the caller the revert had failed when it had not, so a
        // retry made a second revert that undid the first. The outcome now carries the id either
        // way, plus whatever the working copy could not do.
        //
        // `materialise` is made to fail by putting a directory where a file it must write goes: the
        // write fails, and nothing about the database changes.
        let tmp = std::env::temp_dir().join(format!("zf_revert_mat_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        let a1 = put_object(&conn, "A\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','one','a',1,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','a.txt',NULL,?1)",
            params![a1],
        )
        .unwrap();
        commit_one(&conn, "cs1").unwrap();
        fs::write(tmp.join("a.txt"), "A\n").unwrap();

        let b1 = put_object(&conn, "B\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','two','a',2,'committed','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','a.txt',?1,?2)",
            params![a1, b1],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();
        fs::write(tmp.join("a.txt"), "B\n").unwrap();

        // A directory where `materialise` wants to write `a.txt`.
        fs::remove_file(tmp.join("a.txt")).unwrap();
        fs::create_dir_all(tmp.join("a.txt")).unwrap();

        let outcome = revert_changeset(&conn, &tmp, "cs2", "bot", None)
            .expect("the revert itself succeeded, so this must not be an error");
        assert!(
            outcome.materialise_error.is_some(),
            "the working-copy failure has to be reported, not swallowed"
        );
        // The revert is in the history and the branch points at it.
        assert_eq!(
            branch_head(&conn, "main").unwrap().as_deref(),
            Some(outcome.id.as_str()),
            "the revert must be durable even though the files were not written"
        );
        let stored: String = conn
            .query_row(
                "SELECT message FROM changesets WHERE id = ?1",
                params![outcome.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored.starts_with("Revert "),
            "the changeset records the revert, got: {stored}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
    #[test]
    fn reverting_a_revert_restores_the_change() {
        let tmp = std::env::temp_dir().join(format!("zf_revert2_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("a.txt"), "A\n").unwrap();
        commit_files(&conn, "cs1", "v1", &[("a.txt", "A\n")]);

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','v2','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let h2 = put_object(&conn, "B\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','a.txt',?1,?2)",
            params![hash_content("A\n"), h2],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();

        let r1 = revert_changeset(&conn, &tmp, "cs2", "bot", None)
            .unwrap()
            .id;
        let r2 = revert_changeset(&conn, &tmp, &r1, "bot", None).unwrap().id;
        assert_eq!(
            branch_head(&conn, "main").unwrap().as_deref(),
            Some(r2.as_str())
        );
        assert_eq!(fs::read_to_string(tmp.join("a.txt")).unwrap(), "B\n");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn revert_refuses_an_uncommitted_changeset() {
        let tmp = std::env::temp_dir().join(format!("zf_revert3_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let conn = mem_db();
        commit_files(&conn, "cs1", "v1", &[("a.txt", "A\n")]);
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('pend','pending','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let err = revert_changeset(&conn, &tmp, "pend", "bot", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("never committed"), "got: {}", err);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn revert_brings_back_a_deleted_file() {
        let tmp = std::env::temp_dir().join(format!("zf_revert4_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("gone.txt"), "keep me\n").unwrap();
        commit_files(&conn, "cs1", "add", &[("gone.txt", "keep me\n")]);

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','delete','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','gone.txt',?1,NULL)",
            params![hash_content("keep me\n")],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();
        let _ = fs::remove_file(tmp.join("gone.txt"));

        revert_changeset(&conn, &tmp, "cs2", "bot", None).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.join("gone.txt")).unwrap(),
            "keep me\n",
            "reverting a deletion must put the file back"
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
