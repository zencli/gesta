//! Changesets built from content the caller supplies, with no working tree.
//!
//! Two paths use this. A remote agent posts file contents over HTTP and never
//! checks anything out; a test wants to build history without touching disk.
//! The rules are the same either way: compare against what the branch will
//! actually contain, refuse duplicates and contradictions, and never leave an
//! empty changeset behind.

use rusqlite::Connection;

use crate::store::branch_head;
use crate::store::tree_at;
use crate::store::{new_changeset_id, put_object, put_object_bytes};
use crate::{bad, ZResult};

/// A changeset built from caller-supplied content, with no working tree involved.
pub struct ContentChangeset {
    pub id: String,
    pub paths: Vec<String>,
    /// Paths the caller asked to delete that were already absent.
    pub skipped: Vec<String>,
}

/// Create a changeset from explicit file contents.
///
/// This is the path a remote agent takes: it knows the bytes it wants to write
/// and has no checkout, so the blobs are written directly and the changeset is
/// recorded against the branch head. Renames are not inferred here; the caller
/// states what it wrote and what it deleted.
/// The tree a new changeset must be compared against: the branch head with
/// every pending changeset on that branch applied on top, in creation order.
///
/// Comparing against the head alone is wrong when changesets are still pending.
/// A file added by a queued changeset is not in the head tree, so a later
/// changeset that deletes it would be told "nothing to delete", and one that
/// edits it would record the wrong `old_hash`. This is the tree the branch will
/// actually have once the queue drains.
///
/// A `None` value means the path is deleted at that point.
pub fn effective_tree(
    conn: &Connection,
    branch: &str,
) -> ZResult<std::collections::HashMap<String, Option<String>>> {
    let mut out: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();

    if let Some(head) = branch_head(conn, branch)? {
        for (path, hash) in tree_at(conn, &head)? {
            out.insert(path, Some(hash));
        }
    }

    let pending: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets
             WHERE branch = ?1 AND status = 'pending'
             ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(rusqlite::params![branch], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for id in pending {
        let entries: Vec<(String, Option<String>)> = {
            let mut stmt = conn.prepare(
                "SELECT path, new_hash FROM changeset_files WHERE changeset_id = ?1 ORDER BY path",
            )?;
            let rows = stmt.query_map(rusqlite::params![id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (path, new_hash) in entries {
            match new_hash {
                Some(h) => {
                    out.insert(path, Some(h));
                }
                None => {
                    out.remove(&path);
                }
            }
        }
    }

    Ok(out)
}

pub fn changeset_from_content(
    conn: &Connection,
    message: &str,
    author: &str,
    branch: &str,
    files: &[(String, String)],
    delete: &[String],
) -> ZResult<ContentChangeset> {
    let head = branch_head(conn, branch)?;
    // The base includes changesets still pending on this branch, not just the
    // head, so a second API call sees what the first one wrote.
    let committed = effective_tree(conn, branch)?;

    let id = new_changeset_id(conn, message, author)?;
    let now = crate::support::clock::unix_now();

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch)
         VALUES(?1, ?2, ?3, ?4, 'pending', ?5, NULL, ?6)",
        rusqlite::params![id, message, author, now, head, branch],
    )?;

    let mut paths = Vec::new();
    let mut wrote = std::collections::HashSet::new();
    let mut skipped: Vec<String> = Vec::new();

    for (path, content) in files {
        if !wrote.insert(path.clone()) {
            // A duplicate path in one request is ambiguous: two contents for one
            // file. Refusing is the only honest answer.
            return Err(bad(format!("{} appears twice in one changeset", path)));
        }
        // Binary files arrive with empty content; read bytes from disk and store
        // Raw bytes. Text files use the provided string.
        let new_hash = if content.is_empty() {
            let bytes = std::fs::read(path)?;
            put_object_bytes(&tx, &bytes)?
        } else {
            put_object(&tx, content)?
        };
        let old_hash = committed.get(path).cloned().flatten();
        if old_hash.as_deref() == Some(new_hash.as_str()) {
            continue; // identical to the head: nothing to record
        }
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![id, path, old_hash, new_hash],
        )?;
        paths.push(path.clone());
    }

    for path in delete {
        if wrote.contains(path) {
            return Err(bad(format!("{} is both written and deleted", path)));
        }
        // The path has to exist in the effective tree to be deletable. A path
        // that is already gone is not an error, it is a no-op the caller should
        // know about.
        let old_hash = match committed.get(path).cloned().flatten() {
            Some(h) => h,
            None => {
                skipped.push(path.clone());
                continue;
            }
        };
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, NULL)",
            rusqlite::params![id, path, old_hash],
        )?;
        paths.push(path.clone());
    }

    if paths.is_empty() {
        // The changeset row is already written; drop it rather than leave an
        // empty pending changeset behind.
        tx.rollback()?;
        return Err(bad("nothing changed: every file matches the branch head"));
    }

    tx.commit()?;
    Ok(ContentChangeset { id, paths, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::committed_tree;
    use crate::store::tests::mem_db;

    #[test]
    fn a_changeset_records_only_what_differs_from_the_head() {
        let conn = mem_db();
        let first = vec![("a.txt".to_string(), "one\n".to_string())];
        let cs = changeset_from_content(&conn, "add", "agent", "main", &first, &[]).unwrap();
        assert_eq!(cs.paths, vec!["a.txt".to_string()]);

        // The same content again is not a change.
        let again = changeset_from_content(&conn, "same", "agent", "main", &first, &[]);
        assert!(
            again.is_err(),
            "identical content must not create a changeset"
        );
    }

    #[test]
    fn effective_tree_includes_a_pending_changeset() {
        // A file written by a queued changeset is not in the head tree. Anything
        // comparing only against the head would not see it, which is how a
        // second API call lost track of what the first one wrote.
        let conn = mem_db();
        let files = vec![("src/app.py".to_string(), "print(1)\n".to_string())];
        let cs = changeset_from_content(&conn, "queued", "agent", "main", &files, &[]).unwrap();

        let committed = committed_tree(&conn, "main").unwrap();
        assert!(
            !committed.contains_key("src/app.py"),
            "head tree must not have it yet"
        );

        let effective = effective_tree(&conn, "main").unwrap();
        assert!(
            effective.get("src/app.py").cloned().flatten().is_some(),
            "the effective tree must include the pending changeset {}",
            cs.id
        );
    }

    #[test]
    fn a_second_changeset_can_delete_what_the_first_one_added() {
        let conn = mem_db();
        let files = vec![("doc.md".to_string(), "# doc\n".to_string())];
        changeset_from_content(&conn, "add", "agent", "main", &files, &[]).unwrap();

        let del = changeset_from_content(
            &conn,
            "remove",
            "agent",
            "main",
            &[],
            &["doc.md".to_string()],
        )
        .unwrap();
        assert_eq!(del.paths, vec!["doc.md".to_string()]);
        assert!(
            del.skipped.is_empty(),
            "the path existed, so nothing was skipped"
        );

        let effective = effective_tree(&conn, "main").unwrap();
        assert!(
            !effective.contains_key("doc.md"),
            "the delete must remove the path from the effective tree"
        );
    }

    #[test]
    fn deleting_a_path_that_does_not_exist_is_reported_not_silently_dropped() {
        let conn = mem_db();
        let files = vec![("keep.txt".to_string(), "x\n".to_string())];
        let r = changeset_from_content(
            &conn,
            "one",
            "a",
            "main",
            &files,
            &["ghost.txt".to_string()],
        )
        .unwrap();
        assert_eq!(r.paths, vec!["keep.txt".to_string()]);
        assert_eq!(r.skipped, vec!["ghost.txt".to_string()]);
    }

    #[test]
    fn editing_a_pending_file_records_the_pending_hash_as_its_old_hash() {
        // The old_hash has to come from the effective tree, otherwise the second
        // changeset claims to rewrite a version that never existed.
        let conn = mem_db();
        let first = vec![("a.txt".to_string(), "one\n".to_string())];
        changeset_from_content(&conn, "first", "agent", "main", &first, &[]).unwrap();

        let second = vec![("a.txt".to_string(), "two\n".to_string())];
        let cs = changeset_from_content(&conn, "second", "agent", "main", &second, &[]).unwrap();

        let old: Option<String> = conn
            .query_row(
                "SELECT old_hash FROM changeset_files WHERE changeset_id = ?1 AND path = 'a.txt'",
                rusqlite::params![cs.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old.as_deref(),
            Some(crate::store::hash_content("one\n").as_str()),
            "old_hash must be the pending version, not the head version"
        );
    }

    #[test]
    fn an_unknown_path_to_delete_is_refused_when_nothing_else_changed() {
        let conn = mem_db();
        let r = changeset_from_content(&conn, "noop", "a", "main", &[], &["never.txt".to_string()]);
        // Nothing to record, so the changeset is refused rather than left empty.
        assert!(r.is_err());
    }

    #[test]
    fn a_duplicate_path_in_one_request_is_refused() {
        let conn = mem_db();
        let files = vec![
            ("a.txt".to_string(), "one\n".to_string()),
            ("a.txt".to_string(), "two\n".to_string()),
        ];
        let r = changeset_from_content(&conn, "dupe", "a", "main", &files, &[]);
        assert!(r.is_err(), "two contents for one path is ambiguous");
    }

    #[test]
    fn a_path_that_is_both_written_and_deleted_is_refused() {
        let conn = mem_db();
        let files = vec![("a.txt".to_string(), "one\n".to_string())];
        let r = changeset_from_content(
            &conn,
            "conflict",
            "a",
            "main",
            &files,
            &["a.txt".to_string()],
        );
        assert!(r.is_err());
    }
}
