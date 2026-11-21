//! The repository itself: init, mount, mcp, and the guards a commit runs through.

use super::*;

pub fn cmd_init(bare: bool) -> ZResult<()> {
    let root = std::env::current_dir()?;
    let zf = repo_dir(&root);
    if zf.is_dir() {
        // The directory can exist with no database inside it: a restore that failed part way, a
        // half copied project, a `.gesta` left behind after its file was deleted. Reporting "already
        // a repository" then leaves every command failing with "no such table: changesets" and no
        // way to recover except deleting the directory by hand.
        let conn = open_db(&root)?;
        let has_schema: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='changesets'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_schema > 0 {
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
    Ok(())
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
        msg.push_str(&format!("  and {} more\n", found.len() - 10));
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
            Ok(bytes) => Some(crate::store::hash_content(&String::from_utf8_lossy(&bytes))),
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
