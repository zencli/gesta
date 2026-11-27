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
use crate::store::{new_changeset_id, put_object, put_object_bytes_for};
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

/// A repository is a database and a root directory, and functions that need both should say so once.
///
/// It arrived as a bare pair (#107): the database was passed everywhere and the root was not passed
/// at all, which is how a file came to be read against the process's working directory. Naming the
/// pair makes the second half impossible to forget, and it is the reason this function has seven
/// arguments rather than eight.
#[derive(Clone, Copy)]
pub struct Repo<'a> {
    pub conn: &'a Connection,
    pub root: &'a std::path::Path,
}

pub fn changeset_from_content(
    repo: Repo<'_>,
    message: &str,
    author: &str,
    branch: &str,
    files: &[(String, String)],
    delete: &[String],
    rules: Option<&crate::support::attributes::Rules>,
) -> ZResult<ContentChangeset> {
    let conn = repo.conn;
    let root = repo.root;
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
        // Every entry is validated, whether or not its content was sent (#310).
        //
        // The check below this block was inside the `content.is_empty()` arm, so an entry with content
        // went straight into `changeset_files` with no validation at all. The one production caller
        // validates first, which is why nothing was reachable through it - but this function is where
        // a tree is built, and a tree holding `../../x` is the input of every checkout that has to
        // place it: the restore and bisect traversal fixed in #275 read exactly these rows. A store
        // function that trusts its caller to have checked is a store function that cannot be reused
        // safely, and the check is one predicate.
        if !crate::support::paths::is_repo_relative(path) {
            return Err(bad(format!(
                "{}: a tree entry must be a relative path that stays inside the repository. \
                 Refused: it is absolute, climbs out, or is malformed.",
                path
            )));
        }
        // Binary files arrive with empty content; read bytes from disk and store
        // Raw bytes. Text files use the provided string.
        let new_hash = if content.is_empty() {
            // The path was validated above, for both arms, so the reason this function is the place
            // to check is worth keeping: reading `path` as given meant a client could name
            // `/etc/passwd` and have its contents stored as an object, then read back through the
            // ordinary file routes (#152). Opening a file on the caller's word is the sharpest use of
            // the path, which is why the check lives here and not at the HTTP layer.
            // Against the repository root, not against whatever directory the server happens to
            // have been started in (#107). This function is handed a tree path - the same string
            // that goes into `changeset_files` - and reading it as given meant the answer depended
            // on the process's cwd: correct only when the server started inside the repo, a
            // different file or a hard error everywhere else. Nothing in the signature said so, and
            // the comment claimed the caller passes a path on disk, which the one caller never did.
            let on_disk = root.join(path);
            let bytes = std::fs::read(&on_disk).map_err(|e| {
                bad(format!(
                    "{}: an entry with empty content is read from disk, and it could not be read ({e}). \
                     Send the content, or a path that exists inside the repository.",
                    path
                ))
            })?;
            // The rule is written against the path inside the repository; the caller passes the
            // path on disk so the bytes can be read. Look the rule up by the tail that the tree
            // knows, which is the last two components - enough for `*.bin` and `vendor/**`, and
            // honest about being an approximation for a pattern anchored at the root.
            let rel = relative_tail(path);
            put_object_bytes_for(&tx, &bytes, Some(rel.as_str()), rules)?
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

/// The repository-relative tail of a path on disk.
///
/// A `.gitattributes` pattern is written against the tree, and the caller holds an absolute path. A
/// full resolution needs the repository root, which this function is not given; taking the file name
/// and the one directory above covers the patterns people write (`*.bin`, `assets/**`) without
/// pretending to handle an anchor at the root.
fn relative_tail(path: &str) -> String {
    let p = std::path::Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(path);
    match p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
    {
        Some(dir) => format!("{}/{}", dir, name),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::committed_tree;
    use crate::store::tests::mem_db;

    /// A root for tests whose content is never empty: they take the `else` branch, so the only code
    /// that reads through the root is not reached. Explicit rather than reusing `mem_db`, which has
    /// no directory behind it.
    fn no_disk() -> &'static std::path::Path {
        std::path::Path::new("/nonexistent-test-root")
    }

    #[test]
    fn a_repo_relative_path_is_the_only_kind_allowed() {
        for ok in ["a.txt", "src/main.rs", "a/b/c.rs", "assets/x.bin"] {
            assert!(
                crate::support::paths::is_repo_relative(ok),
                "{ok} must be accepted"
            );
        }
        let backslash = "a\\b";
        let unc = "\\\\server\\share\\x";
        for bad in [
            "/etc/passwd",
            "../../etc/shadow",
            "a/../../b",
            unc,
            "C:/Windows/win.ini",
            "..",
            "",
            backslash,
            // Refused since #304: an empty component and `.` are malformed, which is what this
            // function's doc always said.
            "./a.txt",
            "a/",
            "a//b",
        ] {
            assert!(
                !crate::support::paths::is_repo_relative(bad),
                "{bad:?} must be refused as a tree path"
            );
        }
    }

    #[test]
    fn an_empty_content_entry_cannot_make_the_server_read_outside_the_repository() {
        // #152: an entry with empty content is the signal to read the bytes off disk, and the path
        // came from the request body. A client could name a file outside the repository and have its
        // contents stored as an object, then read back through the ordinary file routes.
        let conn = mem_db();
        let outside = vec![("/etc/passwd".to_string(), String::new())];
        let err = match changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "steal",
            "agent",
            "main",
            &outside,
            &[],
            None,
        ) {
            Ok(cs) => panic!("an absolute path was accepted: {}", cs.id),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("must be a relative path") && err.contains("climbs out"),
            "the refusal must say the path is not a tree path: {err}"
        );

        // The climbing form is refused for the same reason.
        let climb = vec![("../../../etc/shadow".to_string(), String::new())];
        let err2 = match changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "steal",
            "agent",
            "main",
            &climb,
            &[],
            None,
        ) {
            Ok(cs) => panic!("a climbing path was accepted: {}", cs.id),
            Err(e) => e.to_string(),
        };
        assert!(err2.contains("climbs out"), "{err2}");
    }

    #[test]
    fn a_changeset_records_only_what_differs_from_the_head() {
        let conn = mem_db();
        let first = vec![("a.txt".to_string(), "one\n".to_string())];
        let cs = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "add",
            "agent",
            "main",
            &first,
            &[],
            None,
        )
        .unwrap();
        assert_eq!(cs.paths, vec!["a.txt".to_string()]);

        // The same content again is not a change.
        let again = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "same",
            "agent",
            "main",
            &first,
            &[],
            None,
        );
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
        let cs = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "queued",
            "agent",
            "main",
            &files,
            &[],
            None,
        )
        .unwrap();

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
        changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "add",
            "agent",
            "main",
            &files,
            &[],
            None,
        )
        .unwrap();

        let del = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "remove",
            "agent",
            "main",
            &[],
            &["doc.md".to_string()],
            None,
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
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "one",
            "a",
            "main",
            &files,
            &["ghost.txt".to_string()],
            None,
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
        changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "first",
            "agent",
            "main",
            &first,
            &[],
            None,
        )
        .unwrap();

        let second = vec![("a.txt".to_string(), "two\n".to_string())];
        let cs = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "second",
            "agent",
            "main",
            &second,
            &[],
            None,
        )
        .unwrap();

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
        let r = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "noop",
            "a",
            "main",
            &[],
            &["never.txt".to_string()],
            None,
        );
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
        let r = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "dupe",
            "a",
            "main",
            &files,
            &[],
            None,
        );
        assert!(r.is_err(), "two contents for one path is ambiguous");
    }

    #[test]
    fn a_path_that_is_both_written_and_deleted_is_refused() {
        let conn = mem_db();
        let files = vec![("a.txt".to_string(), "one\n".to_string())];
        let r = changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "conflict",
            "a",
            "main",
            &files,
            &["a.txt".to_string()],
            None,
        );
        assert!(r.is_err());
    }

    #[test]
    fn a_binary_upload_is_read_from_the_repository_not_the_processs_cwd() {
        // #107. The read was `std::fs::read(path)` with the tree path, so it resolved against
        // whatever directory the server was started in - correct exactly when the cwd happened to
        // be the repo, a wrong file or a hard error otherwise, with nothing in the signature
        // saying so.
        //
        // The repo here is a temp directory and the process does not stand in it, so the old code
        // cannot find the file at all. That is the weaker half of the defect (the wrong-file half
        // needs a cwd that holds a same-named file), and it is the half that can be asserted
        // without `set_current_dir`, which is process-global and would race every other test in
        // this binary.
        let repo = std::env::temp_dir().join(format!("gesta-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("blob.bin"), b"from the repository").unwrap();
        assert!(
            !std::path::Path::new("blob.bin").exists(),
            "this test needs the cwd to NOT hold blob.bin, or it cannot tell the two reads apart"
        );

        let conn = mem_db();
        // Empty content means "read the bytes off disk" - the branch under test.
        let files = vec![("blob.bin".to_string(), String::new())];
        let cs = changeset_from_content(
            Repo {
                conn: &conn,
                root: &repo,
            },
            "bin",
            "agent",
            "main",
            &files,
            &[],
            None,
        )
        .unwrap_or_else(|e| {
            panic!("a binary upload was not read from the repository it was given: {e}")
        });

        let hash: String = conn
            .query_row(
                "SELECT new_hash FROM changeset_files WHERE changeset_id = ?1 AND path = 'blob.bin'",
                rusqlite::params![cs.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hash,
            crate::store::hash_content("from the repository"),
            "the stored object is not the file inside the repository"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
    #[test]
    fn an_entry_with_content_is_validated_too() {
        // #310: the check lived inside the `content.is_empty()` arm, so an entry that SENT its content
        // went into `changeset_files` with no validation at all. The one production caller validated
        // first, which is why nothing was reachable through it - but this is where a tree is built, and
        // a tree holding `../../x` is exactly the input of the checkout paths fixed in #275.
        let conn = mem_db();
        let poisoned = vec![("../../escaped.txt".to_string(), "content".to_string())];
        let err = match changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "poison",
            "agent",
            "main",
            &poisoned,
            &[],
            None,
        ) {
            Ok(cs) => panic!("a climbing path with content was accepted: {}", cs.id),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("must be a relative path"),
            "the refusal must name the rule: {err}"
        );

        // Nothing was written: the tree is still empty.
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM changeset_files", [], |r| r.get(0))
            .unwrap_or(0);
        assert_eq!(rows, 0, "a refused entry must not leave rows behind");

        // An ordinary path with content still works, so the guard cannot pass by refusing everything.
        let fine = vec![("src/ok.txt".to_string(), "content".to_string())];
        changeset_from_content(
            Repo {
                conn: &conn,
                root: no_disk(),
            },
            "fine",
            "agent",
            "main",
            &fine,
            &[],
            None,
        )
        .expect("a normal path with content must be accepted");
    }
}
