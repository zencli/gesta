//! Backing up and restoring a repository.
//!
//! A backup is the database, not a bundle. A bundle carries one branch's history and the objects it
//! reaches, which is what a push needs and is not what a backup needs: tags, blame ranges, the
//! branch tree cache and every branch other than the one named all live outside it. Losing those and
//! calling the result a backup is the failure this module exists to avoid.
//!
//! The copy is taken through SQLite's own backup API rather than by copying the file. A copy of a
//! live database in WAL mode can miss the most recent commits, which sit in the write-ahead log until
//! a checkpoint, and it is the commits made a minute ago that a person is most upset to lose.
//!
//! Restoring verifies before it replaces. A restore that is interrupted half way leaves the
//! repository worse than it started, so the checks run on the backup file first and the existing
//! store is only moved aside once the new one has opened and passed `fsck`.

use crate::ZResult;
use rusqlite::Connection;

/// A failure the caller can act on: a bad file, a bad destination, a store that did not verify.
fn bad_err(msg: String) -> Box<crate::error::Error> {
    Box::new(crate::error::Error::new(crate::error::Code::Corrupt, msg))
}

/// What a backup reported, so the caller can say something specific./// What a backup reported, so the caller can say something specific.
pub struct BackupReport {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub changesets: i64,
    pub objects: i64,
    pub branches: i64,
    pub tags: i64,
}

/// A summary of what a store holds, for comparing a backup with its source.
pub struct Contents {
    pub changesets: i64,
    pub objects: i64,
    pub branches: i64,
    pub tags: i64,
    pub file_versions: i64,
    pub blame_ranges: i64,
}

/// Count the rows a person would notice the loss of.
///
/// One question, not six, and the table names are written out. A helper that took a table name and
/// formatted it into the statement is the shape the project refuses everywhere else, and a list of
/// tables is short enough that writing them costs nothing.
pub fn contents(conn: &Connection) -> ZResult<Contents> {
    let counts: (i64, i64, i64, i64, i64, i64) = conn.query_row(
        "SELECT
           (SELECT COUNT(*) FROM changesets),
           (SELECT COUNT(*) FROM objects),
           (SELECT COUNT(*) FROM branches),
           (SELECT COUNT(*) FROM tags),
           (SELECT COUNT(*) FROM file_versions),
           (SELECT COUNT(*) FROM blame_ranges)",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    Ok(Contents {
        changesets: counts.0,
        objects: counts.1,
        branches: counts.2,
        tags: counts.3,
        file_versions: counts.4,
        blame_ranges: counts.5,
    })
}

/// Write the whole store to `dest`.
///
/// `dest` is replaced if it exists, because a backup that silently appends to an older file produces
/// a file that is neither the old snapshot nor the new one.
pub fn backup(conn: &Connection, dest: &std::path::Path) -> ZResult<BackupReport> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }

    // Counted before the copy starts. The copy is consistent as of when it begins, so a writer that
    // commits during the copy leaves the file correctly holding fewer rows than the live store holds
    // now. Comparing against the live count at the end made every backup of a busy repository fail,
    // which is the one moment a backup has to work.
    let before = contents(conn)?;

    {
        let mut out = Connection::open(dest)?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        out.pragma_update(None, "page_size", page_size)?;
        // The backup reads the source in one pass. A source being written to during the copy is
        // handled by SQLite's locking: the copy sees a consistent state as of when it started.
        let copy = rusqlite::backup::Backup::new(conn, &mut out)?;
        copy.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
    }

    // The backup API writes a database without WAL settings, so the result opens as a clean store.
    let check = Connection::open(dest)?;
    let after = contents(&check)?;
    // Against the count taken before the copy, not the live one.
    if after.changesets < before.changesets || after.objects < before.objects {
        return Err(bad_err(format!(
            "the backup holds fewer rows than the repository held when the copy started: {} changesets against {}, {} objects against {}",
            after.changesets, before.changesets, after.objects, before.objects
        )));
    }
    drop(check);

    let bytes = std::fs::metadata(dest)?.len();
    Ok(BackupReport {
        path: dest.to_path_buf(),
        bytes,
        changesets: after.changesets,
        objects: after.objects,
        branches: after.branches,
        tags: after.tags,
    })
}

/// Check a backup file without touching the repository it will replace.
///
/// Every reason a restore could fail after it has already moved the live store aside is a reason to
/// check first. This opens the file, reads it, and runs the same integrity questions `fsck` runs.
pub fn verify(file: &std::path::Path) -> ZResult<Contents> {
    if !file.exists() {
        return Err(crate::backup::bad_err(format!(
            "no backup at {}",
            file.display()
        )));
    }
    let conn = Connection::open(file).map_err(|e| {
        crate::backup::bad_err(format!("{} is not a database: {}", file.display(), e))
    })?;

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| {
            crate::backup::bad_err(format!("{} could not be read: {}", file.display(), e))
        })?;
    if integrity != "ok" {
        return Err(crate::backup::bad_err(format!(
            "{} failed its integrity check: {}",
            file.display(),
            integrity
        )));
    }

    // A file that is a valid database but not a repository is the wrong file, and saying so is
    // better than restoring it and finding out when every command reports a missing table.
    let has_schema: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='changesets'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_schema == 0 {
        return Err(crate::backup::bad_err(format!(
            "{} is a database but not a gesta repository: it has no changesets table",
            file.display()
        )));
    }

    contents(&conn)
}

/// Replace the store at `root` with the one in `file`.
///
/// The live store is moved aside rather than deleted, and the move only happens after the backup has
/// been verified. If installing the new store fails, the old one is put back, so a failed restore
/// leaves a working repository instead of none.
pub fn restore(root: &std::path::Path, file: &std::path::Path) -> ZResult<Contents> {
    // Verify before anything moves. Everything that can be checked is checked here.
    let checked = verify(file)?;

    let db = crate::store::db_path(root);
    let aside = db.with_extension("before-restore");
    let _ = std::fs::remove_file(&aside);

    if db.exists() {
        std::fs::rename(&db, &aside)?;
    }
    // The WAL and shared-memory files belong to the store that just moved. Leaving them beside the
    // new database is how a restore produces a store that opens and then reports no commits.
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{}", db.display(), suffix));
        let _ = std::fs::remove_file(side);
    }

    let installed = (|| -> ZResult<()> {
        std::fs::copy(file, &db)?;
        let conn = Connection::open(&db)?;
        // Reopen through the ordinary path so the schema and pragmas are what the rest of the code
        // expects, and so a backup written by an older version migrates.
        drop(conn);
        let conn = crate::store::open_db(root)?;
        crate::store::init_schema(&conn)?;
        Ok(())
    })();

    match installed {
        Ok(()) => {
            let _ = std::fs::remove_file(&aside);
            Ok(checked)
        }
        Err(e) => {
            // Put the old store back. A restore that fails and leaves nothing is worse than the
            // failure it was trying to fix.
            let _ = std::fs::remove_file(&db);
            if aside.exists() {
                let _ = std::fs::rename(&aside, &db);
            }
            Err(crate::backup::bad_err(format!(
                "the backup could not be installed and the repository was put back as it was: {}",
                e
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(dir: &std::path::Path) -> Connection {
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let conn = crate::store::open_db(dir).unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_backup_carries_what_a_bundle_does_not() {
        let d = std::env::temp_dir().join(format!("gesta-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = repo(&d);

        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        crate::repo::create_changeset(&conn, &d, "first", "t").unwrap();
        let id: String = conn
            .query_row("SELECT id FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        crate::repo::commit_one(&conn, &id).unwrap();

        // A tag and a second branch: neither travels in a bundle, and both are what a person means
        // by "the repository" when they ask for a backup.
        conn.execute(
            "INSERT INTO tags(name, changeset_id, created_at) VALUES('v1', ?1, 0)",
            rusqlite::params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('side', ?1, 0, 'main')",
            rusqlite::params![id],
        )
        .unwrap();

        let dest = d.join("snapshot.db");
        let report = backup(&conn, &dest).unwrap();
        assert_eq!(report.tags, 1, "the tag is in the backup");
        assert_eq!(report.branches, 2, "both branches are in the backup");
        assert!(report.bytes > 0);

        let back = verify(&dest).unwrap();
        assert_eq!(back.changesets, 1);
        assert_eq!(back.tags, 1);
        assert_eq!(back.branches, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_that_is_not_a_repository_is_refused_before_anything_moves() {
        let d = std::env::temp_dir().join(format!("gesta-bk2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let other = d.join("not-a-repo.db");
        let c = Connection::open(&other).unwrap();
        c.execute("CREATE TABLE unrelated(x INTEGER)", []).unwrap();
        drop(c);

        let e = match verify(&other) {
            Ok(_) => panic!("a database with no changesets table was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(e.contains("not a gesta repository"), "unexpected: {}", e);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_file_is_named_rather_than_a_panic() {
        let e = match verify(std::path::Path::new("/nonexistent/x.db")) {
            Ok(_) => panic!("a missing file was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(e.contains("no backup at"), "unexpected: {}", e);
    }

    #[test]
    fn a_gesta_directory_with_no_database_can_be_repaired() {
        // The state a failed restore or a half copied project leaves behind. Before this, `init`
        // saw the directory and said "already a repository", so every command then failed with
        // "no such table: changesets" and the only way out was deleting the directory by hand.
        let d = std::env::temp_dir().join(format!("gesta-bk3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();

        let conn = crate::store::open_db(&d).unwrap();
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='changesets'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 0, "a fresh file has no schema");

        crate::store::init_schema(&conn).unwrap();
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='changesets'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 1, "the schema is installed on demand");
        let _ = std::fs::remove_dir_all(&d);
    }
}
