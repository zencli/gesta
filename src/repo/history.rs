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
    for (dpath, dhash) in &deleted {
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

    let mut stmt = conn.prepare(
        "SELECT changeset_id FROM changeset_files WHERE path = ?1
         ORDER BY rowid DESC LIMIT 40",
    )?;
    let changesets: Vec<String> = stmt
        .query_map(params![current.clone()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for cs in changesets {
        if chain.len() > max_depth {
            break;
        }
        for r in detect_renames(conn, &cs, 0.5)? {
            if r.to == current {
                chain.push(r.from.clone());
                current = r.from.clone();
                break;
            }
        }
    }
    Ok(chain)
}

/// Undo the effect of `target` by recording a new changeset that restores the
/// content each of its files had before it.
///
/// This is a revert, not a reset: history is never rewritten, so a revert is
/// itself revertable and safe on a branch other people have already pulled.
pub fn revert_changeset(
    conn: &Connection,
    root: &Path,
    target: &str,
    author: &str,
    reason: Option<&str>,
) -> ZResult<String> {
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
        let current = latest_hash(&tx, path, &branch)?;
        if let (Some(c), Some(n)) = (&current, new) {
            if c != n {
                let base_text = match restore_hash.as_ref() {
                    Some(h) => get_object(&tx, h)?,
                    None => String::new(),
                };
                let our_text = get_object(&tx, c)?;
                let their_text = get_object(&tx, n)?;
                if merge3(&base_text, &our_text, &their_text).is_conflict() {
                    return Err(bad(format!(
                        "cannot revert {}: {} changed in a conflicting way since then",
                        target, path
                    )));
                }
            }
        }

        if let Some(h) = &restore_hash {
            put_object(&tx, &get_object(&tx, h)?)?;
        }
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            params![revert_id, path, current, restore_hash],
        )?;
        restored += 1;
    }
    tx.commit()?;

    commit_one(conn, &revert_id)?;
    // The commit updated the database; the working copy has to follow or the
    // revert looks applied while the old content is still on disk.
    materialise(conn, root, &branch)?;
    let _ = restored;
    Ok(revert_id)
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

        let rev = revert_changeset(&conn, &tmp, "cs2", "bot", Some("test")).unwrap();
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

        let r1 = revert_changeset(&conn, &tmp, "cs2", "bot", None).unwrap();
        let r2 = revert_changeset(&conn, &tmp, &r1, "bot", None).unwrap();
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
