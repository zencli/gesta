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
    /// Name, head, and where the branch was created from.
    ///
    /// `from_branch` is carried because `restore` wrote NULL for it, so a single undo permanently
    /// erased where every branch came from (#291).
    pub branches: Vec<(String, Option<String>, Option<String>)>,
    pub current_branch: String,
    /// Name, changeset, and the tag message.
    ///
    /// The message is carried for the same reason: `restore` wrote NULL, so undoing anything
    /// discarded every tag's message (#291).
    pub tags: Vec<(String, String, Option<String>)>,
}

/// Escape a snapshot field so a tab or a newline in a name survives the round trip (#89).
///
/// `%` goes first, or `%09` in the input would decode back to a tab.
fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for ch in v.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(v: &str) -> ZResult<String> {
    let mut out = String::with_capacity(v.len());
    let bytes = v.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(bad(format!(
                    "a snapshot field ends inside an escape: {v:?}"
                )));
            }
            let hex = &v[i + 1..i + 3];
            let ch = match hex {
                "25" => '%',
                "09" => '\t',
                "0A" => '\n',
                "0D" => '\r',
                other => {
                    return Err(bad(format!(
                        "a snapshot field holds an escape this format does not define: %{other}"
                    )))
                }
            };
            out.push(ch);
            i += 3;
        } else {
            let ch = v[i..].chars().next().unwrap_or('\u{0}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

impl RepoSnapshot {
    pub fn capture(conn: &Connection) -> ZResult<RepoSnapshot> {
        let branches: Vec<(String, Option<String>, Option<String>)> = {
            let mut stmt =
                conn.prepare("SELECT name, head, from_branch FROM branches ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let tags: Vec<(String, String, Option<String>)> = {
            let mut stmt =
                conn.prepare("SELECT name, changeset_id, message FROM tags ORDER BY name")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
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
        for (name, head, from_branch) in &self.branches {
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET head = excluded.head,
                                                from_branch = excluded.from_branch",
                params![name, head, now(), from_branch],
            )?;
            // Undo moves a head, so the derived tree has to follow. Missing this
            // made `fsck` report a stale tree after any undo, and `status` then
            // reported already-committed files as new.
            crate::store::refresh_branch_tree(conn, name)?;
        }
        // Branches created after the snapshot are removed.
        let keep: Vec<String> = self.branches.iter().map(|(n, _, _)| n.clone()).collect();
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
        for (name, cs, message) in &self.tags {
            conn.execute(
                "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, ?3, ?4)",
                params![name, cs, message, now()],
            )?;
        }
        crate::store::set_state(conn, "current_branch", &self.current_branch)?;
        Ok(())
    }

    fn encode(&self) -> String {
        // `V2` first, then every field escaped (#89). The first format wrote names raw into a
        // tab-separated, line-based text, so a branch named with a tab or a newline produced a line
        // that could not be parsed; `decode` then dropped it and `restore` deleted every branch it
        // did not see. A branch is a name a person can type - `$'a\tb'` is a legal one - so the
        // format has to carry it, not hope it does not appear.
        // `V3` adds the branch origin and the tag message; `V2` is still read, with those two
        // fields absent (#291). A snapshot is written and read by the same binary in practice -
        // it lives inside the `operations` row that the same command wrote - so bumping the marker
        // cannot strand an old snapshot; the reader accepts both anyway, because a rollback to an
        // older gesta must not make its own log unreadable.
        let mut s = String::from("V3\n");
        s.push_str(&format!("B\t{}\n", escape(&self.current_branch)));
        for (n, h, from) in &self.branches {
            s.push_str(&format!(
                "R\t{}\t{}\t{}\n",
                escape(n),
                h.as_deref().map(escape).unwrap_or_else(|| "-".into()),
                from.as_deref().map(escape).unwrap_or_else(|| "-".into())
            ));
        }
        for (n, c, m) in &self.tags {
            s.push_str(&format!(
                "T\t{}\t{}\t{}\n",
                escape(n),
                escape(c),
                m.as_deref().map(escape).unwrap_or_else(|| "-".into())
            ));
        }
        s
    }

    /// Decode a snapshot.
    ///
    /// A line this cannot read is an error, never a line to skip (#89). Skipping is what made the
    /// old format dangerous: `restore` deletes every branch the snapshot does not name, so a branch
    /// dropped here was a branch deleted - silently, by an undo the user asked for to get their work
    /// back. `V2` is escaped and strict; anything else is read as V1, also strictly.
    fn decode(s: &str) -> ZResult<RepoSnapshot> {
        let mut lines = s.lines();
        let first = lines.next().unwrap_or("");
        // `V3` carries the branch origin and the tag message; in `V2` those columns are absent, so
        // the arms below take the short form and leave both `None` - which is what a snapshot written
        // before #291 held anyway (#291).
        let escaped = first == "V2" || first == "V3";
        if !escaped && !first.starts_with('B') {
            return Err(bad(format!(
                "the operation snapshot does not start with a branch line or a V2 marker; the first \
                 line is {:?}",
                first
            )));
        }

        let mut snap = RepoSnapshot {
            branches: Vec::new(),
            current_branch: String::new(),
            tags: Vec::new(),
        };
        let mut saw_branch = false;
        let all = if escaped {
            lines.collect::<Vec<_>>()
        } else {
            std::iter::once(first).chain(lines).collect()
        };
        for line in all {
            let f: Vec<&str> = line.split('\t').collect();
            let un = |v: &str| -> ZResult<String> {
                if escaped {
                    unescape(v)
                } else {
                    Ok(v.to_string())
                }
            };
            match f.as_slice() {
                ["B", b] => {
                    snap.current_branch = un(b)?;
                    saw_branch = true;
                }
                ["R", n, h] => {
                    snap.branches
                        .push((un(n)?, if *h == "-" { None } else { Some(un(h)?) }, None))
                }
                ["R", n, h, from] => snap.branches.push((
                    un(n)?,
                    if *h == "-" { None } else { Some(un(h)?) },
                    if *from == "-" { None } else { Some(un(from)?) },
                )),
                ["T", n, c] => snap.tags.push((un(n)?, un(c)?, None)),
                ["T", n, c, m] => {
                    snap.tags
                        .push((un(n)?, un(c)?, if *m == "-" { None } else { Some(un(m)?) }))
                }
                other => {
                    return Err(bad(format!(
                        "the operation snapshot holds a line this cannot read ({} field(s): {:?});                          refusing rather than skipping it, because `restore` deletes every branch \
                         the snapshot does not name",
                        other.len(),
                        line
                    )));
                }
            }
        }
        if !saw_branch {
            return Err(bad(
                "the operation snapshot has no current-branch line; defaulting it would silently \
                 move the user onto a branch they were not on",
            ));
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
    // Every step inside one transaction (#86). Reading `MAX(id)` and inserting afterwards, on two
    // connections, lets both read the same parent and insert under it, forking the chain `undo`
    // walks by id order. Both snapshots survive, so this is not data loss - it is undo returning
    // operations in an order that is not the order they happened, which is worse than an error
    // because it looks like an answer.
    // IMMEDIATE, not the deferred default (#86). Deferred takes the write lock only at the first
    // write, so two records can both read `MAX(id)` under a shared lock and then both try to
    // upgrade - which SQLite answers with `database is locked`, not with a wait. Measured: 8 threads
    // x 5 records gave `DatabaseBusy` on every thread. IMMEDIATE takes the write lock up front, so
    // the second caller waits and reads the parent the first one committed.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = record_in(conn, kind, target, detail);
    match outcome {
        Ok(id) => {
            conn.execute_batch("COMMIT")?;
            Ok(id)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Record an operation inside a transaction the caller already holds.
///
/// A caller that is rewriting history (#85) has its own transaction open, and starting a second one
/// on the same connection is an error, not a nested savepoint. This is the part that has to be
/// inside the caller's transaction so the log entry and the change land together.
pub fn record_in(
    tx: &rusqlite::Connection,
    kind: &str,
    target: Option<&str>,
    detail: Option<&str>,
) -> ZResult<i64> {
    let snap = RepoSnapshot::capture(tx)?;
    let parent: Option<i64> = tx
        .query_row("SELECT MAX(id) FROM operations", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten();
    tx.execute(
        "INSERT INTO operations(parent_op, kind, target, detail, snapshot, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![parent, kind, target, detail, snap.encode(), now()],
    )?;
    Ok(tx.last_insert_rowid())
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
            branches: vec![("main".into(), None, None)],
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
    //
    // A failure here is reported, not swallowed (#88). `unwrap_or(0)` returned success and the
    // message "undid ... (operation N, 0 file(s) written)" after the head had already moved and the
    // disk had not: the exact inconsistency this step exists to prevent, announced as a success. The
    // operation cannot be rolled back at this point - it is committed and the head is on the old
    // state - so the message says what is true: the log moved, the files did not.
    let branch: String = crate::store::current_branch(conn)?;
    let written = match crate::repo::materialise(conn, root, &branch) {
        Ok(n) => n,
        Err(e) => {
            return Err(bad(format!(
            "operation {} was undone in the log, but the working tree could not be brought back \
                 to branch '{}' and the files on disk no longer match the head: {}. The undo is in \
                 the log; run `gesta status` to see what the tree is missing.",
            id, branch, e
        )))
        }
    };

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
    // The newest undo is the row with the LOWEST id among the undone ones (#272).
    //
    // Undo marks rows in descending order: operations 1..5, undo takes 5, then 4. So after two
    // undos the undone rows are {4, 5} and the most recent undo is 4, not 5. Taking the highest
    // replayed 5 first: there is no undone=0 row after 5, so the fallback jumped the head straight
    // to that changeset while operation 4 was still undone, and `log` and the branch disagreed.
    // Redo is a stack, so it pops in the order the undos pushed.
    let next: Option<(i64, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id, kind, target, snapshot FROM operations
             WHERE undone = 1 ORDER BY id ASC LIMIT 1",
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
    // Reported for the same reason as in `undo` (#88): the redo is already committed, so a silent
    // failure here leaves the head ahead of the disk and calls it a success.
    let written = crate::repo::materialise(conn, root, &branch).map_err(|e| {
        bad(format!(
            "operation {} was redone in the log, but the working tree could not be brought back to \
             branch '{}' and the files on disk no longer match the head: {}",
            id, branch, e
        ))
    })?;
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
            branches: vec![
                ("main".into(), Some("cs1".into()), None),
                ("dev".into(), None, Some("main".into())),
            ],
            current_branch: "dev".into(),
            tags: vec![("v1.0".into(), "cs1".into(), Some("release".into()))],
        };
        let back = RepoSnapshot::decode(&snap.encode()).unwrap();
        assert_eq!(back.current_branch, "dev");
        assert_eq!(back.branches.len(), 2);
        assert_eq!(
            back.tags,
            vec![(
                "v1.0".to_string(),
                "cs1".to_string(),
                Some("release".to_string())
            )]
        );
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
    #[test]
    fn undo_reports_failure_when_the_disk_cannot_follow_the_head() {
        // #88: `undo` and `redo` committed the head move, then called `materialise(..).unwrap_or(0)`
        // and reported success - "undid ... (operation N, 0 file(s) written)" - with the head moved
        // and the disk not following. That is exactly the inconsistency the step exists to prevent,
        // announced as a success. The undo cannot be rolled back at that point, so the honest answer
        // is an error that says the log moved and the tree did not.
        let conn = mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-undo-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A real tree, through the same path the rest of the code uses, so `materialise` has an
        // object to write. Writing `changeset_files` by hand leaves `file_versions` empty, the tree
        // is then empty, and `materialise` returns 0 legitimately - which is what the first version
        // of this test measured.
        std::fs::write(dir.join("blocked.txt"), "one\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "first", "t").unwrap();
        let id: String = conn
            .query_row("SELECT id FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        crate::repo::commit_one(&conn, &id).unwrap();
        record(&conn, "commit", Some(&id), None).unwrap();

        // A directory where the file must be written makes the write fail.
        std::fs::remove_file(dir.join("blocked.txt")).unwrap();
        std::fs::create_dir_all(dir.join("blocked.txt")).unwrap();

        // The undo now has work: the head is on `cs1`, and undoing it puts the branch back to empty,
        // which means `materialise` must remove `blocked.txt` - and cannot, because a directory is
        // in its place. Removing a file that is not a file is not an error in `materialise`, so make
        // the write fail instead: a second commit that ADDS a file, whose path is the directory.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','m','a',2,'committed',?1,'main')",
            rusqlite::params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES('cs2','blocked.txt', NULL, 'h-blocked')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_versions(path, changeset_id, hash, created_at, version)
             VALUES('blocked.txt','cs2','h-blocked',2,2)",
            [],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head = 'cs2' WHERE name = 'main'", [])
            .unwrap();
        record(&conn, "commit", Some("cs2"), None).unwrap();

        match undo(&conn, &dir) {
            Ok(msg) => panic!("the disk could not follow the head, yet undo reported: {msg}"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("working tree could not be brought back"),
                    "the error must say the tree did not follow, got: {msg}"
                );
                assert!(
                    msg.contains("undone in the log"),
                    "and that the head did move, or the reader will retry the wrong thing: {msg}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn two_records_at_once_do_not_fork_the_chain() {
        // #86: `record` captured, read MAX(id) and inserted as three separate statements, so two
        // concurrent records could read the same parent and both insert under it. The chain `undo`
        // walks by id order then has a fork, and undo order stops being the order things happened.
        //
        // The check: record from many threads against one file-backed store (a shared cache would
        // serialise in memory and hide the race), then assert the operations form one chain - one
        // root, every other row pointing at an id that exists, and no two rows sharing a parent.
        let dir = std::env::temp_dir().join(format!("gesta-ops-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::store::init_schema(&conn).unwrap();
        }

        let threads = 8;
        let mut handles = Vec::new();
        for t in 0..threads {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let conn = rusqlite::Connection::open(&path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(20))
                    .unwrap();
                for i in 0..5 {
                    record(&conn, "commit", Some(&format!("cs-{t}-{i}")), None).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let conn = rusqlite::Connection::open(&path).unwrap();
        let roots: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE parent_op IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roots, 1, "one chain has one root, found {roots}");

        // No two operations may share a parent: that is the fork.
        let shared: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (
                   SELECT parent_op FROM operations
                   WHERE parent_op IS NOT NULL
                   GROUP BY parent_op HAVING COUNT(*) > 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            shared, 0,
            "{shared} parent(s) have more than one child: the chain forked"
        );

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, threads * 5, "every record must land");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_branch_name_with_a_tab_survives_a_snapshot_round_trip() {
        // #89: `encode` wrote names raw into a tab-separated, line-based text, and `decode` dropped
        // any line it could not read. `restore` deletes every branch the snapshot does not name, so a
        // branch named with a tab was deleted by the undo the user asked for to get their work back.
        //
        // A tab is reachable: `gesta branch create $'a\tb'` passes the name through unvalidated.
        let conn = mem_db();
        let awkward = "topic\twith\ttabs";
        // `mem_db` seeds `main`; the awkward name is the only one this test adds.
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, 1, NULL)",
            params![awkward],
        )
        .unwrap();

        let snap = RepoSnapshot::capture(&conn).unwrap();
        let text = snap.encode();
        assert!(
            !text.contains(awkward),
            "the raw name must not appear in the encoding, or a tab is a field separator again:\n{text}"
        );

        let back = RepoSnapshot::decode(&text).unwrap();
        let names: Vec<&str> = back.branches.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(
            names.contains(&awkward),
            "the branch must survive the round trip, decoded names were {names:?}"
        );

        // And through `restore`: the branch must still exist afterwards, not be deleted as unknown.
        back.restore(&conn).unwrap();
        let still: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branches WHERE name = ?1",
                params![awkward],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            still, 1,
            "restore deleted the branch it could not decode; that is #89 exactly"
        );
    }

    #[test]
    fn a_snapshot_line_that_cannot_be_read_is_refused_not_skipped() {
        // The other half of #89: skipping the line is what turned a parse problem into a deletion.
        let text = "V2\nB\tmain\nR\tbroken-line-with-no-field-separator\n";
        let err = match RepoSnapshot::decode(text) {
            Ok(_) => panic!("a line with no field separator must not decode"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("cannot read"),
            "a line the format cannot read must be an error, got: {err}"
        );

        let no_branch = match RepoSnapshot::decode("V2\nR\tmain\t-\n") {
            Ok(_) => panic!("a snapshot with no current branch must not decode"),
            Err(e) => e,
        };
        assert!(
            no_branch.to_string().contains("no current-branch line"),
            "a snapshot with no current branch must not default to one, got: {no_branch}"
        );
    }
    #[test]
    fn redo_replays_the_oldest_undo_first() {
        // #272: `redo` took `ORDER BY id DESC`, so after two undos it replayed the LAST undone
        // operation instead of the first. Undo marks rows in descending order (5, then 4), so the
        // oldest undone row is the undo the user most recently performed: redo is a stack. With DESC
        // the first redo jumped the head straight past the still-undone row, and `log` and the branch
        // disagreed.
        let d = std::env::temp_dir().join(format!("gesta-redo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, ?1, NULL)",
            params![0i64],
        )
        .unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();

        // Three commits, each followed by an operation record that names the changeset - exactly what
        // the CLI writes. `commit_one` on its own records nothing, and the target matters: replaying
        // the last undone operation uses it to move the head, because there is no later effective row
        // whose snapshot describes what that operation produced.
        let mut heads = Vec::new();
        for i in 1..=3 {
            std::fs::write(d.join("f.txt"), format!("rev{i}\n")).unwrap();
            let cs = crate::repo::create_changeset(&conn, &d, &format!("c{i}"), "alice")
                .unwrap()
                .expect("a written file is something to snapshot");
            crate::repo::commit_one(&conn, &cs.id).unwrap();
            crate::support::ops::record(&conn, "commit", Some(&cs.id), None).unwrap();
            heads.push(cs.id);
        }

        // Undo twice: pops op3 then op2, leaving the head at the first commit.
        crate::support::ops::undo(&conn, &d).unwrap();
        crate::support::ops::undo(&conn, &d).unwrap();
        let after_undo = crate::store::branch_head(&conn, "main").unwrap().unwrap();
        assert_eq!(after_undo, heads[0], "two undos leave the first commit");

        // One redo must replay op2 - the older undone row - not op3.
        crate::support::ops::redo(&conn, &d).unwrap();
        let after_one = crate::store::branch_head(&conn, "main").unwrap().unwrap();
        assert_eq!(
            after_one, heads[1],
            "the first redo replays the undo the user performed most recently, which is op2"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "rev2\n",
            "and the disk follows it"
        );

        // The second redo reaches the newest commit.
        crate::support::ops::redo(&conn, &d).unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap().unwrap(),
            heads[2],
            "the second redo replays op3"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_snapshot_carries_the_tag_message_and_the_branch_origin() {
        // #291: `capture` read only (name, changeset) for tags and (name, head) for branches, and
        // `restore` wrote NULL for the two columns it did not carry. One undo therefore erased every
        // tag's message and every branch's origin for good.
        let d = std::env::temp_dir().join(format!("gesta-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch)
             VALUES('main', NULL, 0, NULL), ('dev', NULL, 0, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags(name, changeset_id, message, created_at) VALUES('v1.0', 'cs1', 'release one', 0)",
            [],
        )
        .unwrap();

        let snap = RepoSnapshot::capture(&conn).unwrap();
        // Wipe both pieces of metadata, then put the snapshot back.
        conn.execute("DELETE FROM tags", []).unwrap();
        conn.execute("DELETE FROM branches", []).unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        snap.restore(&conn).unwrap();

        let message: Option<String> = conn
            .query_row("SELECT message FROM tags WHERE name = 'v1.0'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            message.as_deref(),
            Some("release one"),
            "the tag message must survive the round trip"
        );

        let from: Option<String> = conn
            .query_row(
                "SELECT from_branch FROM branches WHERE name = 'dev'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            from.as_deref(),
            Some("main"),
            "the branch origin must survive the round trip"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_v2_snapshot_without_the_new_columns_still_decodes() {
        // Snapshots written before #291 have three fields on a branch line and two on a tag line, and
        // a binary rolled back to an older build must not find its own log unreadable.
        let v2 = "V2\nB\tmain\nR\tdev\tcs1\t-\nT\tv1.0\tcs1\n";
        let snap = RepoSnapshot::decode(v2).unwrap();
        assert_eq!(
            snap.branches,
            vec![("dev".to_string(), Some("cs1".to_string()), None)]
        );
        assert_eq!(
            snap.tags,
            vec![("v1.0".to_string(), "cs1".to_string(), None)]
        );
    }
}
