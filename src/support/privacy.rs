//! Strip or replace the identity carried by existing changesets.
//!
//! A changeset records who wrote it, and that string travels with every push. Replacing it after
//! the fact is the only way to undo a disclosure that already happened, which is a different job
//! from not disclosing in the first place: `remote inspect` answers the second, this answers the
//! first.
//!
//! Two properties decide the design.
//!
//! A signature covers the author, because the author is part of what a reviewer is told the
//! signer vouched for. Rewriting the author therefore invalidates the signature, and leaving a
//! stale signature behind would turn a verified changeset into one that fails verification while
//! looking signed. So the rewrite drops the signature and says how many it dropped.
//!
//! The changeset id does not depend on the author: it is derived when the changeset is created and
//! never recomputed. That is what makes a rewrite possible at all, since everything from parent
//! pointers to remote refs to the branch head is expressed in terms of those ids. Rewriting the
//! author leaves every one of them true.

use rusqlite::Connection;

use crate::{bad, ZResult};

/// What a rewrite changed.
#[derive(Debug)]
pub struct Report {
    /// Changesets whose author was replaced.
    pub rewritten: usize,
    /// Signatures dropped because they covered the old author.
    pub dropped_signatures: usize,
    /// Distinct author strings that were replaced.
    pub authors: Vec<String>,
}

/// Replace the author of every changeset that carries one of the given strings.
///
/// An empty `match_these` means every changeset on the branch is rewritten, which is the case a
/// person wants after pushing from a machine configured with the wrong identity.
pub fn rewrite_authors(
    conn: &Connection,
    branch: &str,
    match_these: &[String],
    replacement: &str,
) -> ZResult<Report> {
    if replacement.trim().is_empty() {
        return Err(bad("the replacement author cannot be empty"));
    }
    if replacement.contains('@') || replacement.contains('<') {
        // Swapping one address for another is not anonymising, and calling it that would let
        // somebody believe the payload is clean when it is not.
        return Err(bad(format!(
            "the replacement author {:?} still looks like an identity: an address or an angle \
             bracket names a person the same way the old one did",
            replacement
        )));
    }

    let mut stmt = conn.prepare(
        "SELECT id, author, signature FROM changesets WHERE branch = ?1 ORDER BY created_at",
    )?;
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map(rusqlite::params![branch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let wanted = |author: &str| -> bool {
        if match_these.is_empty() {
            // Everything except a string that already carries no identity is worth rewriting;
            // rewriting it again would just add noise to the operation log.
            author.contains('@') || author.contains('<') || !author.is_empty()
        } else {
            match_these.iter().any(|m| m == author)
        }
    };

    let mut report = Report {
        rewritten: 0,
        dropped_signatures: 0,
        authors: Vec::new(),
    };

    for (id, author, signature) in rows {
        if !wanted(&author) {
            continue;
        }
        if !report.authors.contains(&author) {
            report.authors.push(author.clone());
        }
        // The signature covered the old author, so it cannot be kept. Dropping it is the honest
        // outcome: the changeset is no longer signed, rather than signed by somebody who did not
        // write it.
        if signature.is_some() {
            report.dropped_signatures += 1;
        }
        conn.execute(
            "UPDATE changesets SET author = ?2, signature = NULL, signer = NULL WHERE id = ?1",
            rusqlite::params![id, replacement],
        )?;
        report.rewritten += 1;
    }

    if report.rewritten == 0 {
        return Ok(report);
    }

    // The operation log records the rewrite so that undo can step back over it like any other
    // change. Without this, the rewrite would be the one mutation with no way back.
    crate::support::ops::record(
        conn,
        "rewrite-authors",
        Some(branch),
        Some(&format!(
            "{} changeset(s) now authored by {}; {} signature(s) dropped",
            report.rewritten, replacement, report.dropped_signatures
        )),
    )?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn
    }

    fn insert(conn: &Connection, id: &str, author: &str, signature: Option<&str>) {
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, branch, signature)
             VALUES(?1, 'm', ?2, 1, 'committed', 'main', ?3)",
            rusqlite::params![id, author, signature],
        )
        .unwrap();
    }

    #[test]
    fn a_replacement_that_is_itself_an_identity_is_refused() {
        let conn = repo();
        insert(&conn, "cs1", "old@example.com", None);
        let err = rewrite_authors(&conn, "main", &[], "new@example.com").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("still looks like an identity"), "{}", text);
    }

    #[test]
    fn a_signature_is_dropped_because_it_covered_the_old_author() {
        let conn = repo();
        insert(&conn, "cs1", "old@example.com", Some("sig"));
        let report = rewrite_authors(&conn, "main", &[], "anonymous").unwrap();
        assert_eq!(report.rewritten, 1);
        assert_eq!(report.dropped_signatures, 1);
        let (author, sig): (String, Option<String>) = conn
            .query_row(
                "SELECT author, signature FROM changesets WHERE id='cs1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(author, "anonymous");
        assert!(
            sig.is_none(),
            "a signature over the old author cannot survive"
        );
    }

    #[test]
    fn only_the_named_authors_are_touched() {
        let conn = repo();
        insert(&conn, "cs1", "keep@example.com", None);
        insert(&conn, "cs2", "go@example.com", None);
        let report =
            rewrite_authors(&conn, "main", &["go@example.com".to_string()], "anonymous").unwrap();
        assert_eq!(report.rewritten, 1);
        let kept: String = conn
            .query_row("SELECT author FROM changesets WHERE id='cs1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, "keep@example.com");
    }

    #[test]
    fn a_second_run_over_the_same_rows_reports_nothing() {
        let conn = repo();
        insert(&conn, "cs1", "old@example.com", None);
        rewrite_authors(&conn, "main", &["old@example.com".to_string()], "anonymous").unwrap();
        let again =
            rewrite_authors(&conn, "main", &["old@example.com".to_string()], "anonymous").unwrap();
        assert_eq!(
            again.rewritten, 0,
            "nothing carries the old string any more"
        );
    }
}
