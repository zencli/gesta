//! Notes attached to a changeset.
//!
//! A changeset is immutable and the thing worth saying about it usually arrives afterwards: which
//! ticket it closes, who reviewed it, why it was reverted. Editing the changeset to carry that would
//! change its hash and break every reference to it, so the note lives beside the changeset and points
//! at it. This is the same trade `git notes` makes, for the same reason.
//!
//! One note per changeset, replaced rather than appended: a list would need ordering and merging that
//! nobody asked for, and the second note is almost always a correction of the first.

use rusqlite::{Connection, OptionalExtension};

use crate::{bad, now, ZResult};

fn ensure(conn: &Connection) -> ZResult<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS notes (
            changeset_id TEXT PRIMARY KEY,
            body         TEXT NOT NULL,
            author       TEXT NOT NULL,
            updated_at   INTEGER NOT NULL
        )",
        [],
    )?;
    Ok(())
}

/// Attach a note, replacing any note the changeset already carries.
///
/// A note against a changeset that does not exist is refused. Storing it anyway would create a row
/// nothing can ever read back, and the caller would have no way to notice the typo in the id.
pub fn add(conn: &Connection, changeset: &str, body: &str, author: &str) -> ZResult<()> {
    ensure(conn)?;
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM changesets WHERE id = ?1",
        rusqlite::params![changeset],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(bad(format!("no changeset {changeset}")));
    }
    conn.execute(
        "INSERT INTO notes (changeset_id, body, author, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(changeset_id) DO UPDATE SET
             body = excluded.body,
             author = excluded.author,
             updated_at = excluded.updated_at",
        rusqlite::params![changeset, body, author, now()],
    )?;
    Ok(())
}

/// The note a changeset carries, if any.
pub fn show(conn: &Connection, changeset: &str) -> ZResult<Option<(String, String)>> {
    ensure(conn)?;
    let row = conn
        .query_row(
            "SELECT body, author FROM notes WHERE changeset_id = ?1",
            rusqlite::params![changeset],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        // `optional`, not `.ok()`: only "no row" means no note. A locked or corrupt database also
        // makes `query_row` fail, and reporting that as "no note" would tell the author their note
        // is gone when the database simply could not answer.
        .optional()?;
    Ok(row)
}

/// Drop a note. The changeset is untouched; that is the point of keeping them apart.
pub fn remove(conn: &Connection, changeset: &str) -> ZResult<bool> {
    ensure(conn)?;
    let n = conn.execute(
        "DELETE FROM notes WHERE changeset_id = ?1",
        rusqlite::params![changeset],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::commit_files;

    #[test]
    fn a_note_against_a_changeset_that_does_not_exist_is_refused() {
        let conn = crate::store::tests::mem_db();
        let err = add(&conn, "cs000000000000", "orphan", "t").unwrap_err();
        assert!(format!("{err}").contains("no changeset"), "got: {err}");
        // Nothing was stored, so a later `show` finds nothing rather than a row nothing can read.
        assert!(show(&conn, "cs000000000000").unwrap().is_none());
    }

    #[test]
    fn a_database_error_is_reported_not_read_as_no_note() {
        // `.ok()` mapped every failure to `None`, so a locked or corrupt database told the author
        // their note was gone. Only "no row" means no note; anything else has to surface.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        add(&conn, "c1", "keep me", "alice").unwrap();
        // A `notes` table without `author` makes the read fail for a reason that is not "no row".
        conn.execute("DROP TABLE notes", []).unwrap();
        conn.execute(
            "CREATE TABLE notes (changeset_id TEXT PRIMARY KEY, body TEXT NOT NULL)",
            [],
        )
        .unwrap();

        let err = show(&conn, "c1").unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("author"),
            "the database error must surface, got: {err}"
        );
    }

    #[test]
    fn a_second_note_replaces_the_first_rather_than_piling_up() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        add(&conn, "c1", "first", "alice").unwrap();
        add(&conn, "c1", "second", "bob").unwrap();
        let (body, author) = show(&conn, "c1").unwrap().expect("a note");
        assert_eq!(body, "second");
        assert_eq!(author, "bob");
    }

    #[test]
    fn a_note_does_not_change_the_changeset_it_belongs_to() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "the original message", &[("f", "1\n")]);
        let before: (String, String) = conn
            .query_row(
                "SELECT message, author FROM changesets WHERE id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        add(&conn, "c1", "closes TICKET-42", "alice").unwrap();
        let after: (String, String) = conn
            .query_row(
                "SELECT message, author FROM changesets WHERE id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            before, after,
            "the changeset is not rewritten by its annotation"
        );
    }

    #[test]
    fn removing_a_note_leaves_the_changeset_and_says_whether_there_was_one() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        add(&conn, "c1", "note", "alice").unwrap();
        assert!(remove(&conn, "c1").unwrap(), "the first removal finds it");
        assert!(!remove(&conn, "c1").unwrap(), "the second does not");
        let msg: String = conn
            .query_row("SELECT message FROM changesets WHERE id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(msg, "one", "the changeset outlives its note");
    }
}
