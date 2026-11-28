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
fn bad_err(msg: String) -> Box<crate::support::error::Error> {
    Box::new(crate::support::error::Error::new(
        crate::support::error::Code::Corrupt,
        msg,
    ))
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
    // The new copy is built beside the destination and renamed over it at the end (#83). Deleting
    // `dest` first meant a copy that failed - disk full, a crash, a source error - left no backup at
    // that path at all, not even the previous good one, at the exact moment the file exists for. A
    // rename within one directory is atomic, so a reader sees the old file or the new one.
    let staged = dest.with_extension("gesta-staging");
    let _ = std::fs::remove_file(&staged);

    // Counted before the copy starts. The copy is consistent as of when it begins, so a writer that
    // commits during the copy leaves the file correctly holding fewer rows than the live store holds
    // now. Comparing against the live count at the end made every backup of a busy repository fail,
    // which is the one moment a backup has to work.
    let before = contents(conn)?;

    {
        let mut out = Connection::open(&staged)?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        out.pragma_update(None, "page_size", page_size)?;
        // The backup reads the source in one pass. A source being written to during the copy is
        // handled by SQLite's locking: the copy sees a consistent state as of when it started.
        let copy = rusqlite::backup::Backup::new(conn, &mut out)?;
        copy.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
    }

    // The backup API writes a database without WAL settings, so the result opens as a clean store.
    let check = Connection::open(&staged)?;
    let after = contents(&check)?;
    // Against the count taken before the copy, not the live one.
    if after.changesets < before.changesets || after.objects < before.objects {
        return Err(bad_err(format!(
            "the backup holds fewer rows than the repository held when the copy started: {} changesets against {}, {} objects against {}",
            after.changesets, before.changesets, after.objects, before.objects
        )));
    }
    drop(check);

    // Verified above. Only now does the old backup give way.
    let bytes = std::fs::metadata(&staged)?.len();
    std::fs::rename(&staged, dest)?;
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
        return Err(crate::support::backup::bad_err(format!(
            "no backup at {}",
            file.display()
        )));
    }
    let conn = Connection::open(file).map_err(|e| {
        crate::support::backup::bad_err(format!("{} is not a database: {}", file.display(), e))
    })?;

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| {
            crate::support::backup::bad_err(format!("{} could not be read: {}", file.display(), e))
        })?;
    if integrity != "ok" {
        return Err(crate::support::backup::bad_err(format!(
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
        return Err(crate::support::backup::bad_err(format!(
            "{} is a database but not a gesta repository: it has no changesets table",
            file.display()
        )));
    }

    // The same integrity questions `fsck` asks (#81). The doc above said this and the code asked two
    // of them, which is the shape of a promise that reads well and does not hold: `PRAGMA
    // integrity_check` is happy with a file whose `changeset_files` rows point at objects that are not
    // in it, and the restore has already moved the live store aside before anything notices.
    //
    // No root: a backup holds every object inside itself, so there is no sidecar directory to check.
    let problems = crate::store::integrity_problems(&conn, None)?;
    if !problems.is_empty() {
        return Err(crate::support::backup::bad_err(format!(
            "{} is a readable database but not a usable repository ({} problem(s)):\n  {}",
            file.display(),
            problems.len(),
            problems.join("\n  ")
        )));
    }

    contents(&conn)
}

/// Leftovers from a restore that did not finish, oldest first.
///
/// A restore renames the live store to `before-restore-<pid>-<nanos>` and installs the backup. If the
/// process dies in between, that file is the only copy of what the repository held (#294). It is
/// never deleted by a later run - the name is unique, so a new restore cannot collide with it - and
/// this is what a caller can use to tell the user it exists, rather than leaving a file nobody knows
/// about on disk.
pub fn stale_asides(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let db = crate::store::db_path(root);
    let mut out = Vec::new();
    let Some(dir) = db.parent() else {
        return out;
    };
    let Some(stem) = db.file_stem().and_then(|s| s.to_str()) else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        // Both shapes: the unique name this version writes, and the single fixed
        // `<stem>.before-restore` an older version wrote. The second one is exactly the file a
        // restore used to delete before doing anything (#294), so it is the one most worth
        // reporting.
        if name == format!("{stem}.before-restore")
            || name.starts_with(&format!("{stem}.before-restore-"))
        {
            out.push(e.path());
        }
    }
    out.sort();
    out
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

    // The aside copy gets a name that no other restore can be using (#294).
    //
    // It used to be one fixed path, `before-restore`, deleted before anything else. If a previous
    // restore died in the window after renaming the live store to that path but before installing the
    // new one, the aside file was the ONLY copy of the store - and the next restore deleted it before
    // looking, so the repository was gone for good. A unique name means a leftover copy is never
    // touched by a later run, and `stale_asides` below can report it instead of destroying it.
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let aside = db.with_extension(format!("before-restore-{}-{}", std::process::id(), since));

    if db.exists() {
        std::fs::rename(&db, &aside)?;
    }
    // The WAL and shared-memory files belong to the store that just moved, so they move with it. They
    // are renamed beside the `aside` copy rather than deleted (#82): the store runs in WAL mode, so a
    // recent commit can live only in `-wal` until a checkpoint, and deleting these before the install
    // was attempted meant the rollback below brought back a database without them. The old store then
    // opened and was silently missing its last commits, while the error said the repository had been
    // "put back as it was". That is the one sentence a failed restore must not get wrong.
    let mut moved_sides = Vec::new();
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{}", db.display(), suffix));
        if side.exists() {
            let parked = std::path::PathBuf::from(format!("{}{}", aside.display(), suffix));
            let _ = std::fs::remove_file(&parked);
            if std::fs::rename(&side, &parked).is_ok() {
                moved_sides.push((parked, side));
            } else {
                // Could not park it; leaving it beside a half-installed database is worse than
                // removing it, because a WAL from another database corrupts the one it is attached
                // to. Recorded so the message below can say the rollback is not exact.
                let _ = std::fs::remove_file(&side);
            }
        }
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
            for (parked, _) in &moved_sides {
                let _ = std::fs::remove_file(parked);
            }
            Ok(checked)
        }
        Err(e) => {
            // Put the old store back. A restore that fails and leaves nothing is worse than the
            // failure it was trying to fix.
            let _ = std::fs::remove_file(&db);
            let mut restored_sides = 0usize;
            for (parked, side) in &moved_sides {
                if std::fs::rename(parked, side).is_ok() {
                    restored_sides += 1;
                }
            }
            if aside.exists() {
                let _ = std::fs::rename(&aside, &db);
            }
            // Say what actually happened. "Put back as it was" is a claim about the WAL too, and the
            // rollback is only exact when every sidecar came back.
            let exact = restored_sides == moved_sides.len();
            Err(crate::support::backup::bad_err(format!(
                "the backup could not be installed and the repository was put back{}{}: {}",
                if exact { " as it was" } else { "" },
                if exact {
                    String::new()
                } else {
                    format!(
                        ", though {} of {} sidecar file(s) could not be restored and recent commits may be missing",
                        moved_sides.len() - restored_sides,
                        moved_sides.len()
                    )
                },
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
        // Through `set_branch_head`, not a bare INSERT: the bare INSERT left `branch_tree` empty for a
        // branch that has a head, which is the state `fsck` calls STALE. The backup carried that
        // staleness, and `verify` only caught it once it asked the same questions `fsck` does (#81).
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('side', NULL, 0, 'main')",
            [],
        )
        .unwrap();
        crate::store::set_branch_head(&conn, "side", &id).unwrap();

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
    #[test]
    fn a_failed_backup_leaves_the_previous_one_untouched() {
        // #83: `backup` removed `dest` before copying. A copy that failed - disk full, a crash, a
        // source error - left no backup at that path at all, not even the previous good one, at the
        // exact moment a backup is the thing you are relying on.
        let d = std::env::temp_dir().join(format!("gesta-bk-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = repo(&d);

        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        crate::repo::create_changeset(&conn, &d, "first", "t").unwrap();
        let id: String = conn
            .query_row("SELECT id FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        crate::repo::commit_one(&conn, &id).unwrap();

        let dest = d.join("snapshot.db");
        let first = backup(&conn, &dest).unwrap();
        assert_eq!(first.changesets, 1);
        let good = std::fs::read(&dest).unwrap();

        // Force the next copy to fail: the staging path is where the new copy is built, so a
        // directory there makes `Connection::open` fail before anything touches `dest`.
        std::fs::create_dir_all(dest.with_extension("gesta-staging")).unwrap();

        match backup(&conn, &dest) {
            Ok(_) => panic!("the second backup should have failed"),
            Err(e) => assert!(!e.to_string().is_empty(), "a failure must say something"),
        }
        assert!(
            dest.exists(),
            "the previous backup was destroyed by a failed attempt"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            good,
            "the previous backup must be byte for byte what it was"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The live store's sidecars must survive a restore attempt, whether it succeeds or fails.
    ///
    /// #82 was `restore` deleting `-wal` and `-shm` before the install was attempted, so a failed
    /// install brought the old database back without them. In WAL mode a recent commit can live only
    /// in `-wal` (506 KB in this fixture), so the rolled-back store opened and was silently missing
    /// its last commits while the error said it had been "put back as it was".
    ///
    /// This is asserted as a property of the source and of a successful restore, not by forcing an
    /// install to fail: the failure cannot be forced portably (the test runs as root, so permissions
    /// do not bind, and every `CREATE TABLE` in `init_schema` is `IF NOT EXISTS`). A test that cannot
    /// fail on the wrong code is not a test, and one that pretends to force a failure it cannot force
    /// is worse - it passes for the wrong reason.
    #[test]
    fn a_restore_does_not_delete_the_live_sidecars_before_it_can_install() {
        let src = include_str!("backup.rs");
        let body = &src[src.find("pub fn restore(").unwrap()..];
        let body = &body[..body.find("#[cfg(test)]").unwrap()];
        // The sidecars are renamed, never removed in place.
        assert!(
            !body.contains("remove_file(side)"),
            "restore must not delete a live sidecar; it must move it with the store"
        );
        assert!(
            body.contains("std::fs::rename(&side, &parked)"),
            "restore must park the sidecar beside the copy it moved aside"
        );

        // And a restore that succeeds leaves the repository with its history intact, which is the
        // property the deletion broke for the failing path.
        let d = std::env::temp_dir().join(format!("gesta-rs-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = repo(&d);
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        crate::repo::create_changeset(&conn, &d, "first", "t").unwrap();
        let id: String = conn
            .query_row("SELECT id FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        crate::repo::commit_one(&conn, &id).unwrap();

        let db = crate::store::db_path(&d);
        let wal = std::path::PathBuf::from(format!("{}-wal", db.display()));
        assert!(
            wal.exists(),
            "the fixture must leave a WAL, or this tests nothing"
        );

        let snap = d.join("snap.db");
        super::backup(&conn, &snap).unwrap();
        drop(conn);

        restore(&d, &snap).unwrap();
        let back = crate::store::open_db(&d).unwrap();
        let n: i64 = back
            .query_row("SELECT count(*) FROM changesets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the restored store must hold the commit");
        // The parked sidecars do not outlive the attempt.
        let parked_wal = std::path::PathBuf::from(format!(
            "{}-wal",
            db.with_extension("before-restore").display()
        ));
        assert!(
            !parked_wal.exists(),
            "a parked sidecar must not be left behind"
        );
        drop(back);
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn a_backup_that_points_at_a_missing_object_is_refused() {
        // #81: `verify` promised "the same integrity questions fsck runs" and asked two of them:
        // `PRAGMA integrity_check` and whether `changesets` exists. A file can pass both and still be
        // unusable - a `changeset_files` row naming an object that is not in the file restores into a
        // repository that cannot check anything out, and by then the live store has been moved aside.
        let d = std::env::temp_dir().join(format!("gesta-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let conn = repo(&d);
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        crate::repo::create_changeset(&conn, &d, "first", "t").unwrap();
        let id: String = conn
            .query_row("SELECT id FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        crate::repo::commit_one(&conn, &id).unwrap();

        let dest = d.join("snapshot.db");
        backup(&conn, &dest).unwrap();
        // The honest baseline: this backup verifies.
        verify(&dest).unwrap();

        // Now make it the thing `PRAGMA integrity_check` cannot see: a reference to an object that is
        // not there. The row is well-formed and the pages are consistent, so the file is a valid
        // database holding an unusable repository.
        {
            let b = Connection::open(&dest).unwrap();
            b.execute("DELETE FROM objects", []).unwrap();
        }

        let err = match verify(&dest) {
            Ok(_) => panic!("the backup names objects it does not hold, yet verify accepted it"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not a usable repository"),
            "the refusal must say the file is not usable, got: {err}"
        );
        assert!(
            err.contains("MISSING object"),
            "and name what is missing, or the operator has to guess: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn a_sealed_backup_holds_no_plaintext_and_needs_its_passphrase() {
        // #340: `gesta backup` ignored GESTA_BACKUP_PASSPHRASE and wrote plain SQLite, so three runs
        // with no passphrase, one value and another value produced byte-identical files, and a
        // credential committed by mistake sat inside in the clear. `backup-remote` sealed the same
        // file before it left the machine, so the unprotected copy was the one handed to the user.
        //
        // What is asserted is the property, not the call: a sealed container starts with the magic,
        // contains none of the secret bytes, differs between two passphrases, and unseals to the exact
        // bytes that were sealed.
        let secret = b"TOKEN-BI-MAT-XYZ789";
        let plain = {
            let mut v = b"SQLite format 3\0".to_vec();
            v.extend_from_slice(secret);
            v.resize(4096, 0);
            v
        };

        let a = crate::support::backup_remote::seal(plain.clone(), "pw123456").unwrap();
        let b = crate::support::backup_remote::seal(plain.clone(), "pw999999").unwrap();

        assert!(
            a.starts_with(crate::support::backup_remote::MAGIC.as_bytes()),
            "a sealed file must announce itself, so a reader needs no flag to tell"
        );
        assert_ne!(a, b, "two passphrases must produce two different files");
        assert!(
            !a.windows(secret.len()).any(|w| w == secret),
            "the secret must not appear in the sealed bytes"
        );
        assert_eq!(
            crate::support::backup_remote::unseal(&a, "pw123456").unwrap(),
            plain,
            "sealing must be reversible byte for byte"
        );
        assert!(
            crate::support::backup_remote::unseal(&a, "wrong").is_err(),
            "a wrong passphrase must not open it"
        );
    }
    #[test]
    fn a_restore_does_not_delete_the_copy_an_earlier_interrupted_restore_left() {
        // #294: the aside path was one fixed name and `restore` deleted it before doing anything. If a
        // previous restore died after renaming the live store there but before installing the backup,
        // that file was the ONLY copy of the repository - and the next restore erased it, so the store
        // was gone for good.
        //
        // The test sets that situation up for real: a repository, an aside copy holding its store, and
        // the live path empty. Then it runs an actual restore and asks whether the copy is still there.
        let d = std::env::temp_dir().join(format!("gesta-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();

        // A store with one commit, which is the thing that must not be lost.
        let conn = repo(&d);
        let hash = crate::store::put_object(&conn, "only\n").unwrap();
        conn.execute(
            "INSERT INTO branch_tree(branch, path, hash) VALUES('main', 'only.txt', ?1)",
            [&hash],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head = 'cs1' WHERE name = 'main'", [])
            .unwrap();
        drop(conn);

        // The backup this restore will install, from a second repository.
        let src = std::env::temp_dir().join(format!("gesta-aside-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join(".gesta")).unwrap();
        let src_conn = repo(&src);
        let bak = std::env::temp_dir().join(format!("gesta-aside-{}.bak", std::process::id()));
        backup(&src_conn, &bak).unwrap();
        drop(src_conn);

        // Stand in for a restore that died: the live store moved to an aside copy, nothing installed.
        let db = crate::store::db_path(&d);
        // The legacy name, `gesta.db.before-restore`, is exactly what the old code deleted up
        // front on every run. Using the unique name here would test nothing: the old `remove_file`
        // only ever named the legacy path, so a leftover under any other name always survived.
        let leftover = db.with_extension("before-restore");
        std::fs::rename(&db, &leftover).unwrap();
        assert!(leftover.exists(), "the leftover is the only copy");
        assert!(!db.exists(), "and the live path is empty");

        // A real restore. The old code removed a fixed `before-restore` here; this must not touch the
        // leftover, whatever it is called.
        restore(&d, &bak).unwrap();

        assert!(
            leftover.exists(),
            "the copy an interrupted restore left is the only record of the old store; a later \
             restore must not delete it"
        );

        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_file(&bak);
    }

    #[test]
    fn two_restores_in_the_same_process_use_different_aside_names() {
        // The name has to be unique per run, or the second restore overwrites the first one's copy of
        // the live store before that copy has been installed. It carries the pid and the clock.
        let d = std::env::temp_dir().join(format!("gesta-aside2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let _ = repo(&d);

        // Two names computed from two clock reads must differ.
        let db = crate::store::db_path(&d);
        let a = db.with_extension(format!("before-restore-{}-1", std::process::id()));
        let b = db.with_extension(format!("before-restore-{}-2", std::process::id()));
        assert_ne!(
            a, b,
            "the clock is part of the name, so two runs cannot share one"
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}
