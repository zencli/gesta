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

use crate::ZResult;

/// Mark a branch protected or not. Returns whether the branch exists.
pub fn set_protected(conn: &Connection, branch: &str, protected: bool) -> ZResult<bool> {
    let n = conn.execute(
        "UPDATE branches SET protected = ?2 WHERE name = ?1",
        params![branch, protected as i64],
    )?;
    Ok(n > 0)
}

pub fn is_protected(conn: &Connection, branch: &str) -> ZResult<bool> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT protected FROM branches WHERE name = ?1",
            params![branch],
            |r| r.get(0),
        )
        .ok();
    Ok(v.unwrap_or(0) != 0)
}

/// Branches that are protected, for a message that names them rather than saying "a branch".
pub fn protected_branches(conn: &Connection) -> ZResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM branches WHERE protected = 1 ORDER BY name")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record one sign-off. Signing twice is not an error: the row is already the answer.
pub fn approve(conn: &Connection, changeset: &str, branch: &str, signer: &str) -> ZResult<()> {
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
