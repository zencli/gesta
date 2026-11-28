//! Protected branches: a write to one needs a sign-off from a different credential.
//!
//! The question this answers is not "may this token write" but "was this change read by somebody
//! other than the person who wrote it". A gate that accepts the writer's own approval is a formality,
//! so an approval names its signer and a signer that matches the pusher does not count.
//!
//! Approvals attach to a changeset, not to a branch. A branch moves, and an approval of "whatever is
//! on main" would silently approve the next change that lands there.

use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OptionalExtension;

use crate::ZResult;

/// Mark a branch protected or not. Returns whether the branch exists.
pub fn set_protected(conn: &Connection, branch: &str, protected: bool) -> ZResult<bool> {
    let n = conn.execute(
        "UPDATE branches SET protected = ?2 WHERE name = ?1",
        params![branch, protected as i64],
    )?;
    Ok(n > 0)
}

/// Whether a branch is protected.
///
/// A database error is an error, not "not protected" (#93). `.ok()` collapsed two different
/// answers - "the store says this branch is not protected" and "the store could not be asked" -
/// into the same `false`, so a branch whose protection could not be read was writable. A guard
/// that fails open on the one path where it is needed is not a guard: the caller here is about to
/// decide whether to refuse a destructive operation.
pub fn is_protected(conn: &Connection, branch: &str) -> ZResult<bool> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT protected FROM branches WHERE name = ?1",
            params![branch],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.unwrap_or(0) != 0)
}

/// Branches that are protected, for a message that names them rather than saying "a branch".
pub fn protected_branches(conn: &Connection) -> ZResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM branches WHERE protected = 1 ORDER BY name")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record one sign-off. Signing twice is not an error: the row is already the answer.
///
/// A changeset that does not exist IS an error (#307). `INSERT OR IGNORE` suppresses every
/// constraint failure, not just the duplicate key, so it also swallowed the foreign-key violation from
/// approving something that is not in the database: the command printed success, recorded nothing, and
/// the caller believed a sign-off existed. A protection rule that counts approvals then found none and
/// refused for a reason nobody could see.
///
/// The existence check is explicit rather than a tightened insert because the two answers must stay
/// different: duplicate is fine, unknown is not.
pub fn approve(conn: &Connection, changeset: &str, branch: &str, signer: &str) -> ZResult<()> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM changesets WHERE id = ?1",
        params![changeset],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(crate::bad(format!(
            "no changeset {} to approve; check the id, or the changeset may not have been committed \
             on this machine",
            changeset
        )));
    }
    conn.execute(
        "INSERT OR IGNORE INTO approvals(changeset_id, branch, signer, created_at)
         VALUES(?1, ?2, ?3, ?4)",
        params![changeset, branch, signer, crate::now()],
    )?;
    Ok(())
}

/// Who has signed off on a changeset, in the order they did.
pub fn approvers(conn: &Connection, changeset: &str) -> ZResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT signer FROM approvals WHERE changeset_id = ?1 ORDER BY created_at")?;
    let rows = stmt.query_map(params![changeset], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Was this changeset signed off by somebody other than `pusher`?
///
/// The pusher's own approval does not count. A repository where one person writes and approves leaves
/// no second reader, and calling that a review is the failure this rule exists to prevent.
pub fn approved_by_another(conn: &Connection, changeset: &str, pusher: &str) -> ZResult<bool> {
    let signers = approvers(conn, changeset)?;
    Ok(signers.iter().any(|s| s != pusher))
}

/// The gate a push to `branch` has to pass.
///
/// `pusher` is the name behind the token that is writing. The error names what is missing, because a
/// refusal that says "denied" sends somebody to read the source to find out which rule fired.
pub fn check_push(conn: &Connection, branch: &str, changeset: &str, pusher: &str) -> ZResult<()> {
    if !is_protected(conn, branch)? {
        return Ok(());
    }
    // A server with no tokens calls every pusher "anonymous", and an anonymous pusher cannot be told
    // apart from a reviewer. A gate that accepts an approval in that case is open to the one person
    // it was meant to stop, so a protected branch needs named credentials.
    if pusher == "anonymous" {
        return Err(crate::support::error::Error::new(
            crate::support::error::Code::Auth,
            format!(
                "{} is protected and this server runs without tokens, so the pusher has no name and an approval cannot be checked against it; add a token with `gesta remote token add write`",
                branch
            ),
        )
        .into());
    }
    if approved_by_another(conn, changeset, pusher)? {
        return Ok(());
    }
    let signers = approvers(conn, changeset)?;
    let why = if signers.is_empty() {
        format!(
            "{} is protected and this change has no approval; a token with the approve role has to sign it off first (protected: {})",
            branch,
            protected_branches(conn)?.join(", ")
        )
    } else {
        format!(
            "{} is protected and the only approval is from the pusher ({}); a second credential has to sign it off",
            branch, pusher
        )
    };
    // ZResult boxes its error, so the message is wrapped rather than returned directly.
    Err(crate::support::error::Error::new(crate::support::error::Code::Auth, why).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_only() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::init_schema(&conn).unwrap();
        // `protected` arrives through `migrate`, not through the CREATE TABLE, so a fixture that
        // only calls `init_schema` has a branches table with no such column.
        crate::store::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn protection_that_cannot_be_read_is_an_error_not_a_no() {
        // #93: `.ok()` turned "the store could not be asked" into `false`, so a branch whose
        // protection was unreadable became writable - on the one path where the answer decides
        // whether to refuse a destructive operation. A guard that fails open is not a guard.
        let conn = schema_only();
        conn.execute("DROP TABLE branches", []).unwrap();

        match is_protected(&conn, "main") {
            Ok(v) => {
                panic!("the table is gone, yet is_protected answered {v}; that is failing open")
            }
            Err(e) => assert!(
                e.to_string().contains("branches"),
                "the error must name what could not be read, got: {e}"
            ),
        }
    }

    #[test]
    fn a_branch_that_is_not_there_is_still_not_protected() {
        // The other half: a missing row is a real answer, not an error, or every command against a
        // branch that does not exist would fail here instead of reporting the branch.
        let conn = schema_only();
        assert!(!is_protected(&conn, "no-such-branch").unwrap());
    }
    #[test]
    fn approving_a_changeset_that_does_not_exist_is_an_error_not_a_silent_success() {
        // #307: `INSERT OR IGNORE` suppresses every constraint failure, not just the duplicate key, so
        // it also swallowed the foreign-key violation from approving something that is not in the
        // database. The command printed success, recorded nothing, and the caller believed a sign-off
        // existed; a protection rule counting approvals then found none and refused for a reason
        // nobody could see.
        let conn = schema_only();

        let err = approve(&conn, "no-such-changeset", "main", "alice")
            .expect_err("an unknown changeset must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("no changeset"),
            "the refusal must say the changeset is unknown, got: {msg}"
        );

        // A real changeset approves, and approving it twice is still not an error.
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, branch)
             VALUES('cs1', 'm', 'a', 1, 'pending', 'main')",
            [],
        )
        .unwrap();
        approve(&conn, "cs1", "main", "alice").unwrap();
        approve(&conn, "cs1", "main", "alice")
            .expect("signing twice is the row already being the answer");
        assert_eq!(approvers(&conn, "cs1").unwrap(), vec!["alice".to_string()]);
    }
}
