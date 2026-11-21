//! Operation log and undo.
//!
//! Every command that moves a ref records a snapshot of all refs afterwards.
//! Undo restores the previous snapshot. Objects never move, so this is cheap:
//! no content is copied, only the pointer table.

use rusqlite::{params, Connection, OptionalExtension};

use crate::{bad, now, ZResult};

/// Snapshot of every ref the log cares about.
#[derive(Clone)]
pub struct RepoSnapshot {
    pub branches: Vec<(String, Option<String>)>,
    pub current_branch: String,
    pub tags: Vec<(String, String)>,
}

impl RepoSnapshot {
    pub fn capture(conn: &Connection) -> ZResult<RepoSnapshot> {
        let branches: Vec<(String, Option<String>)> = {
            let mut stmt = conn.prepare("SELECT name, head FROM branches ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let tags: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT name, changeset_id FROM tags ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let current_branch = crate::store::current_branch(conn)?;
        Ok(RepoSnapshot {
            branches,
            current_branch,
            tags,
        })
    }

    pub fn restore(&self, conn: &Connection) -> ZResult<()> {
        for (name, head) in &self.branches {
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, NULL)
                 ON CONFLICT(name) DO UPDATE SET head = excluded.head",
                params![name, head, now()],
            )?;
            // Undo moves a head, so the derived tree has to follow. Missing this
            // made `fsck` report a stale tree after any undo, and `status` then
            // reported already-committed files as new.
            crate::store::refresh_branch_tree(conn, name)?;
        }
        // Branches created after the snapshot are removed.
        let keep: Vec<String> = self.branches.iter().map(|(n, _)| n.clone()).collect();
        let existing: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM branches")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for name in existing {
            if !keep.contains(&name) {
                conn.execute("DELETE FROM branches WHERE name = ?1", params![name])?;
                conn.execute("DELETE FROM branch_tree WHERE branch = ?1", params![name])?;
            }
        }
        conn.execute("DELETE FROM tags", [])?;
        for (name, cs) in &self.tags {
            conn.execute(
                "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, NULL, ?3)",
                params![name, cs, now()],
            )?;
        }
        crate::store::set_state(conn, "current_branch", &self.current_branch)?;
        Ok(())
    }

    fn encode(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("B\t{}\n", self.current_branch));
        for (n, h) in &self.branches {
            s.push_str(&format!(
                "R\t{}\t{}\n",
                n,
                h.clone().unwrap_or_else(|| "-".into())
            ));
        }
        for (n, c) in &self.tags {
            s.push_str(&format!("T\t{}\t{}\n", n, c));
        }
        s
    }

    fn decode(s: &str) -> ZResult<RepoSnapshot> {
        let mut snap = RepoSnapshot {
            branches: Vec::new(),
            current_branch: "main".into(),
            tags: Vec::new(),
        };
        for line in s.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["B", b] => snap.current_branch = b.to_string(),
                ["R", n, h] => snap.branches.push((
                    n.to_string(),
                    if *h == "-" { None } else { Some(h.to_string()) },
                )),
                ["T", n, c] => snap.tags.push((n.to_string(), c.to_string())),
                _ => {}
            }
        }
        Ok(snap)
    }
}

/// Record an operation. Call AFTER the mutation, with the resulting state.
///
/// Undo restores the snapshot of the operation before this one, which is the only reading
/// that stays true whether a command records before or after it acts. This function records
/// after, so the row describes what the operation produced.
pub fn record(
    conn: &Connection,
    kind: &str,
    target: Option<&str>,
    detail: Option<&str>,
) -> ZResult<i64> {
    let snap = RepoSnapshot::capture(conn)?;
    let parent: Option<i64> = conn
        .query_row("SELECT MAX(id) FROM operations", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten();
    conn.execute(
        "INSERT INTO operations(parent_op, kind, target, detail, snapshot, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![parent, kind, target, detail, snap.encode(), now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub struct Operation {
    pub id: i64,
    pub kind: String,
    pub target: Option<String>,
    pub detail: Option<String>,
    pub created_at: i64,
}

pub fn list(conn: &Connection, limit: usize) -> ZResult<Vec<Operation>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, target, detail, created_at FROM operations
         ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| {
            Ok(Operation {
                id: r.get(0)?,
                kind: r.get(1)?,
                target: r.get(2)?,
                detail: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Undo the most recent operation. Returns a description of what was undone.
///
/// The snapshot of the *previous* operation is what gets restored, so a chain of
/// `undo` calls walks backwards through history one step at a time.
/// Undo the last operation.
///
/// The snapshot restores branch heads and tags. That is only half the state: a head that
/// moves backwards leaves the working tree describing a different commit, so the tree has
/// to be re-materialised or the repository says one thing and the disk another. Three
/// defects lived here, all found by running a real session rather than a test:
///
/// 1. The working tree was never touched, so after undoing a commit that deleted a file,
///    the head claimed the file existed and the disk did not have it. `fsck` was clean,
///    because it compares the stored tree with a rebuild from history and never looks at
///    the disk.
/// 2. The log row was deleted, so a second undo walked further back instead of redoing.
///    Undo claimed to be reversible in both directions and was not.
/// 3. Nothing recorded that an operation had been undone, so `ops` showed a history that
///    no longer matched what happened.
///
/// The fix keeps the row and marks it, so undo becomes a pointer moving along the log, and
/// re-materialises the tree so the disk follows the head.
pub fn undo(conn: &Connection, root: &std::path::Path) -> ZResult<String> {
    // The newest operation still in effect.
    let last: Option<(i64, String, Option<String>)> = conn
        .query_row(
            "SELECT id, kind, target FROM operations WHERE undone = 0 ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let Some((id, kind, target)) = last else {
        return Err(bad(
            "nothing to undo: every operation is already undone".to_string()
        ));
    };

    // The state to restore is the one recorded before this operation took effect. Callers do
    // not agree on when they record (a commit records after moving the head, a branch delete
    // records before removing it), so the previous effective row is the only description of
    // "before" that is true either way.
    let before: Option<String> = conn
        .query_row(
            "SELECT snapshot FROM operations WHERE id < ?1 AND undone = 0
             ORDER BY id DESC LIMIT 1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;

    let tx = conn.unchecked_transaction()?;
    match before {
        Some(s) => RepoSnapshot::decode(&s)?.restore(&tx)?,
        None => RepoSnapshot {
            branches: vec![("main".into(), None)],
            current_branch: "main".into(),
            tags: Vec::new(),
        }
        .restore(&tx)?,
    }
    // The row stays and is marked, so a second undo walks further back and a redo can step
    // forward. Deleting it made undo one way while claiming to be reversible both ways.
    tx.execute(
        "UPDATE operations SET undone = 1 WHERE id = ?1",
        params![id],
    )?;
    tx.commit()?;

    // The head moved, so the disk has to follow. Skipping this left a file the head claims
    // exists missing from the working tree, and `fsck` was clean because it never looks at
    // the disk.
    let branch: String = crate::store::current_branch(conn)?;
    let written = crate::repo::materialise(conn, root, &branch).unwrap_or(0);

    let what = match &target {
        Some(t) => format!("{} {}", kind, t),
        None => kind.clone(),
    };
    Ok(format!(
        "undid {} (operation {}, {} file(s) written)",
        what, id, written
    ))
}

/// Put back the most recently undone operation.
pub fn redo(conn: &Connection, root: &std::path::Path) -> ZResult<String> {
    // The newest undo is the one with the highest id among the undone rows, because undo
    // marks rows in descending order.
    let next: Option<(i64, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id, kind, target, snapshot FROM operations
             WHERE undone = 1 ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    let Some((id, kind, target, _before)) = next else {
        return Err(bad("nothing to redo".to_string()));
    };

    // Redoing means going back to the state this operation produced. That state is the
    // snapshot of the next operation that is still in effect, because that one recorded
    // what it found, which is what this operation left behind.
    let after: Option<String> = conn
        .query_row(
            "SELECT snapshot FROM operations WHERE id > ?1 AND undone = 0
             ORDER BY id ASC LIMIT 1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;

    let tx = conn.unchecked_transaction()?;
    match after {
        Some(s) => RepoSnapshot::decode(&s)?.restore(&tx)?,
        None => {
            // The last operation in the log: restoring what it produced means pointing the
            // branch at the changeset it created.
            let cs = target.clone().filter(|t| t.starts_with("cs"));
            match cs {
                Some(c) => {
                    let branch: String = crate::store::current_branch(conn)?;
                    crate::store::set_branch_head(&tx, &branch, &c)?;
                }
                None => RepoSnapshot::decode(&_before)?.restore(&tx)?,
            }
        }
    }
    tx.execute(
        "UPDATE operations SET undone = 0 WHERE id = ?1",
        params![id],
    )?;
    tx.commit()?;

    let branch: String = crate::store::current_branch(conn)?;
    let written = crate::repo::materialise(conn, root, &branch).unwrap_or(0);
    let what = match &target {
        Some(t) => format!("{} {}", kind, t),
        None => kind.clone(),
    };
    Ok(format!(
        "redid {} (operation {}, {} file(s) written)",
        what, id, written
    ))
}

/// Put back the most recently undone operation.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::mem_db;

    #[test]
    fn snapshot_roundtrips_through_encode() {
        let snap = RepoSnapshot {
            branches: vec![("main".into(), Some("cs1".into())), ("dev".into(), None)],
            current_branch: "dev".into(),
            tags: vec![("v1.0".into(), "cs1".into())],
        };
        let back = RepoSnapshot::decode(&snap.encode()).unwrap();
        assert_eq!(back.current_branch, "dev");
        assert_eq!(back.branches.len(), 2);
        assert_eq!(back.tags, vec![("v1.0".to_string(), "cs1".to_string())]);
    }

    #[test]
    fn record_and_undo_restores_the_previous_head() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undoold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for id in ["cs1", "cs2"] {
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,'m','a',1,'committed',NULL,'main')",
                params![id],
            )
            .unwrap();
        }

        // Exactly what the CLI does: move the head, then record. So each row holds the state
        // the operation produced, and undo walks to the previous row.
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", [])
            .unwrap();
        record(&conn, "commit", Some("cs1"), None).unwrap();
        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", [])
            .unwrap();
        record(&conn, "commit", Some("cs2"), None).unwrap();

        let msg = undo(&conn, &dir).unwrap();
        assert!(msg.contains("commit"), "{}", msg);
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap().as_deref(),
            Some("cs1"),
            "undo must land on the state the first commit produced"
        );

        // The row stays marked, so redo can step forward again.
        redo(&conn, &dir).unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap().as_deref(),
            Some("cs2"),
            "redo must restore the undone operation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_without_operations_is_an_error() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undoerr-{}", std::process::id()));
        let err = undo(&conn, &dir).unwrap_err().to_string();
        assert!(err.contains("nothing to undo"), "got: {}", err);
    }

    #[test]
    fn undo_restores_branch_creation() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undobr-{}", std::process::id()));
        record(&conn, "branch create", Some("dev"), None).unwrap();
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev',NULL,0,'main')",
            [],
        )
        .unwrap();
        record(&conn, "branch create", Some("dev"), None).unwrap();

        undo(&conn, &dir).unwrap();
        let exists: Option<String> = conn
            .query_row("SELECT name FROM branches WHERE name='dev'", [], |r| {
                r.get(0)
            })
            .optional()
            .unwrap();
        assert_eq!(
            exists, None,
            "a branch created after the snapshot is removed by undo"
        );
    }

    #[test]
    fn operations_are_listed_newest_first() {
        let conn = mem_db();
        record(&conn, "commit", Some("cs1"), None).unwrap();
        record(&conn, "branch create", Some("dev"), None).unwrap();
        let ops = list(&conn, 10).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, "branch create");
        assert_eq!(ops[1].kind, "commit");
    }
    /// Undo has to move three things together: the head, the log pointer, and the disk.
    /// Each was broken in a different way, and each is asserted here.
    #[test]
    fn undo_moves_the_head_the_log_and_the_disk_together() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Two commits, the second deleting a file.
        crate::repo::tests::commit_files(
            &conn,
            "u1",
            "one",
            &[("keep.txt", "k\n"), ("gone.txt", "g\n")],
        );
        crate::store::set_branch_head(&conn, "main", "u1").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();
        assert!(dir.join("gone.txt").exists());
        // Recorded at the moment the CLI records: right after the head moved.
        crate::support::ops::record(&conn, "commit", Some("u1"), None).unwrap();

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('u2','two','a',2,'committed','u1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('u2','keep.txt',NULL,NULL)",
            [],
        )
        .unwrap();
        crate::repo::commit_one(&conn, "u2").unwrap();
        // `commit_one` writes the changeset; moving the head is the caller's step, which is
        // what `cmd_changeset_commit` does before recording.
        crate::store::set_branch_head(&conn, "main", "u2").unwrap();
        crate::repo::materialise(&conn, &dir, "main").unwrap();
        assert!(
            !dir.join("keep.txt").exists(),
            "the second commit deletes keep.txt, materialise must remove it"
        );

        crate::support::ops::record(&conn, "commit", Some("u2"), None).unwrap();

        // Undo the second commit: the head goes back and the file comes back.
        undo(&conn, &dir).unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap().as_deref(),
            Some("u1"),
            "undo did not move the head back"
        );
        assert!(
            dir.join("keep.txt").exists(),
            "undo moved the head but left the disk behind"
        );

        // Redo puts it back.
        redo(&conn, &dir).unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap().as_deref(),
            Some("u2"),
            "redo did not move the head forward"
        );
        assert!(
            !dir.join("keep.txt").exists(),
            "redo moved the head but the deleted file stayed on disk"
        );

        // A file the repository never tracked is not touched by any of this.
        std::fs::write(dir.join("mine.txt"), "user data\n").unwrap();
        undo(&conn, &dir).unwrap();
        assert!(
            dir.join("mine.txt").exists(),
            "undo deleted a file it does not own"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_refuses_when_everything_is_already_undone() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undo0-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = undo(&conn, &dir).unwrap_err();
        assert!(err.to_string().contains("nothing to undo"), "got: {}", err);
        let err = redo(&conn, &dir).unwrap_err();
        assert!(err.to_string().contains("nothing to redo"), "got: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Undoing the first changeset of a branch leaves an empty branch, and the disk has to
    /// follow it to empty. It did not: every tracked file stayed, and `status` reported them as
    /// new files, which reads as if the undo had done nothing.
    #[test]
    fn undo_to_an_empty_branch_removes_the_files_the_tree_no_longer_names() {
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undo-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
        std::fs::write(dir.join("d").join("b.txt"), "v2\n").unwrap();

        crate::repo::tests::commit_files(
            &conn,
            "c1",
            "one",
            &[("a.txt", "v1\n"), ("d/b.txt", "v2\n")],
        );
        crate::store::set_branch_head(&conn, "main", "c1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();
        record(&conn, "commit", Some("c1"), None).unwrap();

        undo(&conn, &dir).unwrap();

        assert!(
            !dir.join("a.txt").exists(),
            "a.txt survived an undo to an empty branch"
        );
        assert!(!dir.join("d").join("b.txt").exists(), "d/b.txt survived");
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap(),
            None,
            "the head should be empty"
        );

        // And redo puts both back, which is what makes undo reversible rather than destructive.
        redo(&conn, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "v1\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("d").join("b.txt")).unwrap(),
            "v2\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
