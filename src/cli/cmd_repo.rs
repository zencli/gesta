//! The repository itself: init, mount, mcp, and the guards a commit runs through.

use super::*;

/// Whether this database already holds the schema, asking the database rather than guessing.
///
/// `COUNT(*)` always returns a row, so a failure here means the database could not answer - a lock,
/// a corrupt file. That distinction is the whole point: answering "could not read" with 0 sends the
/// caller to `init_schema`, which writes a fresh schema over a store it never read. Reporting the
/// error leaves the damaged store alone, which is the only recoverable outcome.
///
/// Split out of `cmd_init` so the distinction can be tested without a terminal or a directory.
fn schema_present(conn: &Connection) -> ZResult<bool> {
    let has_schema: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='changesets'",
        [],
        |r| r.get(0),
    )?;
    Ok(has_schema > 0)
}

pub fn cmd_init(bare: bool) -> ZResult<()> {
    let root = std::env::current_dir()?;
    let zf = repo_dir(&root);
    if zf.is_dir() {
        // The directory can exist with no database inside it: a restore that failed part way, a
        // half copied project, a `.gesta` left behind after its file was deleted. Reporting "already
        // a repository" then leaves every command failing with "no such table: changesets" and no
        // way to recover except deleting the directory by hand.
        let conn = open_db(&root)?;
        if schema_present(&conn)? {
            println!("already a repository: {}", zf.display());
            return Ok(());
        }
        init_schema(&conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, ?1, NULL)",
            rusqlite::params![now()],
        )?;
        set_state(&conn, "current_branch", "main")?;
        set_state(&conn, "created_at", &now().to_string())?;
        println!("repaired an empty repository: {}", zf.display());
        return Ok(());
    }
    fs::create_dir_all(&zf)?;
    let conn = open_db(&root)?;
    init_schema(&conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, ?1, NULL)",
        rusqlite::params![now()],
    )?;
    set_state(&conn, "current_branch", "main")?;
    set_state(&conn, "created_at", &now().to_string())?;
    if bare {
        // A bare repository keeps the database in the current directory and no
        // working tree: the scanner refuses to track anything.
        set_state(&conn, "bare", "1")?;
    }
    println!("initialized repository at {}", zf.display());
    println!("branch: main (no changesets yet)");
    Ok(())
}

/// Run the MCP server. stdout belongs to the protocol, so nothing else may
/// write to it while this runs.
pub fn cmd_mcp() -> ZResult<()> {
    let root = repo_root()?;
    mcp::serve_stdio(&root)
}

#[cfg(windows)]
pub fn cmd_mount(target: &str, branch: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = match branch {
        Some(b) => b.to_string(),
        None => current_branch(&conn)?,
    };
    drop(conn);
    crate::store::vfs::mount(&root, &branch, target)
}

/// Scan the content a changeset is about to store, and refuse if it looks like a credential.
///
/// At commit time, on the bytes that are about to become the record, because that is the last moment
/// where refusing costs nothing. After the commit the secret is in the history, and removing it means
/// rewriting a record other people may already have.
///
/// A finding is refused rather than warned about by default. A warning in a stream of output is a
/// warning nobody reads, and the alternative to a refused commit is a command that names the exact
/// entry to add to the allowlist, which takes one line.
#[cfg(not(feature = "regex"))]
pub(crate) fn refuse_secrets(_conn: &Connection, _root: &Path, _id: &str) -> ZResult<()> {
    // A build without the scanner cannot check, and returning `Ok(())` made that a gate that fails
    // open (#324). Every commit path always calls this function, so the missing feature silently
    // turned the commit-time secret check into a no-op while `secret scan` next door answers with a
    // clear Usage error for the same missing feature. Refusing is the only honest answer: a commit
    // this binary cannot check must not look checked.
    Err(Box::new(crate::support::error::Error::new(
        crate::support::error::Code::Usage,
        "this build has no secret scanner, so it cannot check a commit for secrets; \
         rebuild with the `regex` feature, or commit with a build that has it"
            .to_string(),
    )))
}

#[cfg(feature = "regex")]
pub(crate) fn refuse_secrets(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    id: &str,
) -> ZResult<()> {
    let paths: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT path FROM changeset_files WHERE changeset_id = ?1 AND new_hash IS NOT NULL",
        )?;
        let rows = stmt.query_map(rusqlite::params![id], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if paths.is_empty() {
        return Ok(());
    }

    let mut scanner = crate::support::leak::Scanner::default();
    scanner.load_allowlist(root);

    let mut found: Vec<crate::support::leak::Finding> = Vec::new();
    for path in &paths {
        if !crate::support::leak::worth_scanning(path) {
            continue;
        }
        // The bytes the commit would store, not the file on disk: the working tree may have moved
        // on, and the question is what becomes the record.
        let hash: String = match conn.query_row(
            "SELECT new_hash FROM changeset_files WHERE changeset_id = ?1 AND path = ?2",
            rusqlite::params![id, path],
            |r| r.get::<_, Option<String>>(0),
        ) {
            Ok(Some(h)) => h,
            _ => continue,
        };
        let body = match crate::store::get_object(conn, &hash) {
            Ok(b) => b,
            // A binary object is not text and is not scanned; `worth_scanning` covers most of it,
            // and this covers the rest without turning a commit into an error about encoding.
            Err(_) => continue,
        };
        found.extend(scanner.scan_text(path, &body));
    }

    if found.is_empty() {
        return Ok(());
    }

    let mut msg = format!(
        "{} possible secret(s) in this changeset. Nothing was written.\n",
        found.len()
    );
    for f in found.iter().take(10) {
        msg.push_str(&format!("  {}  {}  {}\n", f.path, f.rule, f.redacted));
    }
    if found.len() > 10 {
        // A capped list says where the rest is. A count alone leaves the reader knowing something
        // exists and unable to see it, which matters more here than anywhere: this is the message
        // that stops a commit, so the operator needs the whole list to decide.
        msg.push_str(&format!(
            "  ... and {} more (gesta secret scan lists every one)\n",
            found.len() - 10
        ));
    }
    msg.push_str(
        "\nIf one of these is a fixture rather than a credential, add it to .gesta/secret-allowlist:\n",
    );
    for f in found.iter().take(3) {
        msg.push_str(&format!("  path {}\n", f.path));
    }
    msg.push_str(
        "  rule <id>        turn a rule off for this repository\n  stopword <word>  treat a match as a placeholder\n",
    );
    Err(bad(msg))
}

/// Pending changesets whose recorded content no longer matches the working tree.
///
/// This is what a lost update looks like from the outside: two writers snapshot the same
/// base, one commits, and the other's snapshot now describes a file that has moved on. The
/// commit itself is not wrong at the database level, which is why nothing complained.
pub(crate) fn pending_out_of_date(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    _total: i64,
) -> ZResult<Vec<(String, Vec<String>)>> {
    let pending: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT cf.changeset_id, cf.path
             FROM changesets c JOIN changeset_files cf ON cf.changeset_id = c.id
             WHERE c.status = 'pending' ORDER BY cf.changeset_id, cf.path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut stale: Vec<(String, Vec<String>)> = Vec::new();
    for (id, path) in pending {
        let recorded: Option<String> = conn
            .query_row(
                "SELECT new_hash FROM changeset_files WHERE changeset_id = ?1 AND path = ?2",
                rusqlite::params![id, path],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let on_disk = match std::fs::read(root.join(&path)) {
            // The same recomputation as `brief`, and the same bug: a lossy hash never matches what
            // was recorded for a binary file, so the snapshot is always out of date.
            Ok(bytes) => Some(crate::store::hash_bytes(&bytes)),
            Err(_) => None,
        };
        if recorded != on_disk {
            match stale.iter_mut().find(|(sid, _)| sid == &id) {
                Some((_, paths)) => paths.push(path),
                None => stale.push((id, vec![path])),
            }
        }
    }
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_store_has_no_schema() {
        // The genuine "no schema" answer, so a fix that reported `true` everywhere would fail here.
        // Not `mem_db`: that fixture creates the schema, which is the opposite of what this asks.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        assert!(!schema_present(&conn).unwrap());
        conn.execute(
            "CREATE TABLE changesets (id TEXT PRIMARY KEY, message TEXT NOT NULL)",
            [],
        )
        .unwrap();
        assert!(schema_present(&conn).unwrap());
    }

    #[test]
    fn a_store_that_cannot_be_read_is_an_error_not_an_empty_one() {
        // `unwrap_or(0)` turned every failure into "no schema", and `init_schema` then wrote a fresh
        // schema over a store it had never read. A file that is not a database is the reachable
        // stand-in: SQLite opens the handle and refuses the first query, which is exactly what a
        // corrupt store does.
        let dir = std::env::temp_dir().join(format!("gesta-cmdinit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("not-a-db");
        std::fs::write(&db, b"this is not a SQLite file\n").unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();

        let err = schema_present(&conn);
        assert!(
            err.is_err(),
            "a store that cannot be queried must report the error, not answer false"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[cfg(not(feature = "regex"))]
    #[test]
    fn a_build_without_the_scanner_refuses_a_commit_instead_of_passing_it() {
        // #324: the no-feature `refuse_secrets` returned `Ok(())`, so every commit path - which always
        // calls this - silently lost the secret gate. `secret scan` next door answers with a clear
        // Usage error for the same missing feature; failing open here was the inconsistency, and the
        // dangerous half of it.
        let dir = std::env::temp_dir().join(format!("gesta-nogate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&dir).unwrap();
        crate::store::init_schema(&conn).unwrap();

        let err = refuse_secrets(&conn, &dir, "cs1")
            .expect_err("a build that cannot scan must refuse, not pass");
        let msg = format!("{}", err);
        assert!(
            msg.contains("no secret scanner") || msg.contains("regex"),
            "the refusal has to name the missing feature, got: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
