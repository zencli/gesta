//! Command handlers. Kept out of `main.rs` so the entry point stays a thin
//! dispatcher and no single file grows past the point of review.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::history;
use crate::json::Json;
use crate::mcp;
use crate::merge;
use crate::ops;
use crate::repo;
use crate::store::{
    self, branch_head, current_branch, get_object, hash_at_point, init_schema, open_db,
    reachable_from, set_state,
};
use crate::worktree;
use crate::{bad, fmt_ts, normalise, now, open_repo, repo_dir, repo_root, ZResult};

// init / status / diff / log.

/// `sparse set|list|clear`.
pub fn cmd_sparse(sub: crate::SparseCmd) -> ZResult<()> {
    match sub {
        crate::SparseCmd::List => {
            let (_root, conn) = open_repo()?;
            let scope = crate::sparse::SparseScope::load(&conn)?;
            if scope.is_empty() {
                println!("working set: everything (no sparse scope)");
            } else {
                println!("working set: {} pattern(s)", scope.patterns.len());
                for p in &scope.patterns {
                    println!("  {}", p);
                }
            }
            Ok(())
        }
        crate::SparseCmd::Set { patterns } => {
            let (root, conn) = open_repo()?;
            let scope = crate::sparse::SparseScope {
                patterns: patterns.clone(),
            };
            if scope.is_empty() {
                return Err(bad("give at least one pattern, or use 'sparse clear'"));
            }
            let branch = current_branch(&conn)?;
            let (written, kept) =
                store::with_retry(|| scope.checkout_sparse(&conn, &root, &branch))?;
            scope.save(&conn)?;
            println!(
                "working set: {} pattern(s). {} file(s) written, {} left in the branch but not checked out.",
                scope.patterns.len(),
                written,
                kept
            );
            println!("Files outside the scope are not treated as deleted.");
            Ok(())
        }
        crate::SparseCmd::Clear => {
            let (root, conn) = open_repo()?;
            crate::sparse::SparseScope::clear(&conn)?;
            let branch = current_branch(&conn)?;
            let n = repo::materialise(&conn, &root, &branch)?;
            println!("working set: everything. {} file(s) restored.", n);
            Ok(())
        }
    }
}

/// Run the MCP server. stdout belongs to the protocol, so nothing else may
/// write to it while this runs.
pub fn cmd_mcp() -> ZResult<()> {
    let root = repo_root()?;
    mcp::serve_stdio(&root)
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

/// Show renames recorded by a changeset, so `changeset diff` can label them.
pub fn print_renames(conn: &rusqlite::Connection, id: &str) -> ZResult<()> {
    for r in history::detect_renames(conn, id, 0.5)? {
        println!(
            "  renamed {} -> {} ({:.0}% similar)",
            r.from,
            r.to,
            r.similarity * 100.0
        );
    }
    Ok(())
}

/// What the repository does have, for an error that just said what it does not.
///
/// A message that names the missing value and stops leaves the reader guessing whether they
/// typed it wrong or never made it. Listing what exists answers that in the same line.
fn branch_names(conn: &rusqlite::Connection) -> String {
    let mut stmt = match conn.prepare("SELECT name FROM branches ORDER BY name LIMIT 8") {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let names: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return String::new(),
    };
    if names.is_empty() {
        return "; no branches yet, create one with `gesta branch create <name>`".to_string();
    }
    format!("; known branches: {}", names.join(", "))
}

/// Pending changesets whose recorded content no longer matches the working tree.
///
/// This is what a lost update looks like from the outside: two writers snapshot the same
/// base, one commits, and the other's snapshot now describes a file that has moved on. The
/// commit itself is not wrong at the database level, which is why nothing complained.
fn pending_out_of_date(
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

/// Replace the author on existing changesets.
///
/// The dry run exists because this is not a change to file content: a person asks for it after
/// discovering that an identity went somewhere it should not have, and wants to see the size of the
/// problem before agreeing to it.
pub fn cmd_anonymise(authors: &[String], to: &str, branch: &str, dry_run: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if dry_run {
        // The same query the rewrite uses, so the preview and the action cannot disagree.
        let mut stmt = conn
            .prepare("SELECT id, author FROM changesets WHERE branch = ?1 ORDER BY created_at")?;
        let rows: Vec<(String, String)> = stmt
            .query_map(rusqlite::params![branch], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let hit: Vec<&(String, String)> = rows
            .iter()
            .filter(|(_, a)| {
                if authors.is_empty() {
                    a.contains('@') || a.contains('<') || !a.is_empty()
                } else {
                    authors.iter().any(|m| m == a)
                }
            })
            .collect();
        if hit.is_empty() {
            println!("nothing on {} carries that author", branch);
            return Ok(());
        }
        println!("would rewrite {} changeset(s) on {}", hit.len(), branch);
        let mut seen: Vec<&str> = Vec::new();
        for (_, a) in &hit {
            if !seen.contains(&a.as_str()) {
                seen.push(a.as_str());
            }
        }
        for a in seen {
            println!("  {} -> {}", a, to);
        }
        return Ok(());
    }

    let report = crate::privacy::rewrite_authors(&conn, branch, authors, to)?;
    if report.rewritten == 0 {
        println!("nothing on {} carries that author", branch);
        return Ok(());
    }
    for a in &report.authors {
        println!("  {} -> {}", a, to);
    }
    println!("rewrote {} changeset(s) on {}", report.rewritten, branch);
    if report.dropped_signatures > 0 {
        // Saying this plainly matters: the changesets are no longer signed, and a reader who
        // assumes they still are would trust something that no longer holds.
        println!(
            "  {} signature(s) dropped, because each one covered the author it just lost",
            report.dropped_signatures
        );
    }
    Ok(())
}

/// Mount a branch as a read-only drive. Windows only: the mount is WinFSP.
#[cfg(windows)]
pub fn cmd_mount(target: &str, branch: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = match branch {
        Some(b) => b.to_string(),
        None => current_branch(&conn)?,
    };
    drop(conn);
    crate::vfs::mount(&root, &branch, target)
}

pub fn cmd_status() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    println!("On branch {}", branch);
    match branch_head(&conn, &branch)? {
        Some(h) => println!("HEAD: {}", h),
        None => println!("HEAD: (empty branch)"),
    }

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?;
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM changesets WHERE status = 'pending'",
        [],
        |r| r.get(0),
    )?;
    println!("changesets: {} total, {} pending", total, pending);

    // A pending changeset is a snapshot of an earlier moment. If the file it recorded has
    // changed since, or another writer produced a changeset against the same base, committing
    // it stores content that no longer describes what is on disk. Saying so is the difference
    // between a confusing history and an explainable one.
    if pending > 0 {
        let stale = pending_out_of_date(&conn, &root, total)?;
        if !stale.is_empty() {
            println!(
                "\nwarning: {} pending changeset(s) no longer match the working tree:",
                stale.len()
            );
            for (id, paths) in stale.iter().take(5) {
                println!("  {} ({})", &id[..id.len().min(14)], paths.join(", "));
            }
            println!("Commit or discard them before snapshotting again.");
        }
    }

    let d = repo::diff_worktree(&conn, &root, &branch)?;

    if d.added.is_empty() && d.modified.is_empty() && d.deleted.is_empty() {
        println!("working tree clean ({} files tracked)", d.unchanged);
    } else {
        if !d.added.is_empty() {
            println!("\nnew files ({}):", d.added.len());
            for f in &d.added {
                println!("  A {}", f);
            }
        }
        if !d.modified.is_empty() {
            println!("\nmodified ({}):", d.modified.len());
            for f in &d.modified {
                println!("  M {}", f);
            }
        }
        if !d.deleted.is_empty() {
            println!("\ndeleted ({}):", d.deleted.len());
            for f in &d.deleted {
                println!("  D {}", f);
            }
        }
    }

    let scope = crate::sparse::SparseScope::load(&conn)?;
    if !scope.is_empty() {
        println!(
            "\nsparse: {} pattern(s), files outside it are not treated as deleted",
            scope.patterns.len()
        );
        for p in &scope.patterns {
            println!("  {}", p);
        }
    }

    if d.cached + d.rehashed > 0 {
        println!(
            "\nscan: {} from the stat cache, {} re-hashed",
            d.cached, d.rehashed
        );
    }

    if !d.skipped_binary.is_empty() {
        println!("\nskipped (binary, {} file(s)):", d.skipped_binary.len());
        for f in d.skipped_binary.iter().take(10) {
            println!("  ? {}", f);
        }
        if d.skipped_binary.len() > 10 {
            // Nothing hidden: output that was cut says where the rest is. A count alone leaves
            // the reader knowing something exists and unable to see it.
            println!(
                "  ... and {} more (gesta diff --json lists every skipped path)",
                d.skipped_binary.len() - 10
            );
        }
    }
    Ok(())
}

pub fn cmd_diff(changeset: Option<&str>, name_only: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    match changeset {
        Some(cs) => {
            let diffs = repo::changeset_diff(&conn, cs)?;
            if diffs.is_empty() {
                println!("changeset {} changes no files", cs);
            }
            for (path, removed, added) in diffs {
                if name_only {
                    println!("{}", path);
                    continue;
                }
                println!("\n--- {}", path);
                for l in &removed {
                    println!("-{}", l);
                }
                for l in &added {
                    println!("+{}", l);
                }
            }
        }
        None => {
            // Only the files whose stat entry changed get read and hashed.
            // Scanning and reading every file made `diff` 16x slower than git on
            // a 2000 file repository, because one edited file still meant 2000
            // reads. diff_worktree is the same path `status` uses, so the two
            // commands cannot disagree about what changed either.
            let d = repo::diff_worktree(&conn, &root, &branch)?;
            let mut changed: Vec<String> = Vec::new();
            changed.extend(d.added.iter().cloned());
            changed.extend(d.modified.iter().cloned());
            changed.sort();

            let mut any = false;
            for rel in changed {
                let full = root.join(&rel);
                let Some(content) = worktree::read_text(&full) else {
                    continue;
                };
                let old_hash = store::latest_hash(&conn, &rel, &branch)?;
                any = true;
                if name_only {
                    println!("{}", rel);
                    continue;
                }
                let old_text = match &old_hash {
                    Some(h) => get_object(&conn, h)?,
                    None => String::new(),
                };
                println!("\n--- {}", rel);
                let (removed, added) = merge::line_diff(&old_text, &content);
                for l in &removed {
                    println!("-{}", l);
                }
                for l in &added {
                    println!("+{}", l);
                }
            }
            if !any {
                println!("no changes");
            }
        }
    }
    Ok(())
}

pub fn cmd_log(limit: usize, oneline: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    // One row of the log: id, message, author, time, status, parent, session, intent.
    type LogRow = (
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let all: Vec<LogRow> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at, status, parent, session, intent
             FROM changesets ORDER BY created_at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut n = 0;
    for (id, msg, author, ts, status, parent, session, intent) in all {
        if oneline {
            // The intent rides along on one line, because a reason that needs a second
            // command to see is a reason nobody reads.
            match &intent {
                Some(i) => println!(
                    "{:<14} {} {}  ({})",
                    &id[..id.len().min(14)],
                    fmt_ts(ts),
                    msg,
                    i
                ),
                None => println!("{:<14} {} {}", &id[..id.len().min(14)], fmt_ts(ts), msg),
            }
        } else {
            println!("changeset {:<14}", id);
            println!("Author:  {}", author);
            println!("Date:    {}", fmt_ts(ts));
            println!("Status:  {}", status);
            if let Some(p) = parent {
                println!("Parent:  {}", p);
            }
            if let Some(s) = session {
                println!("Session: {}", s);
            }
            if let Some(i) = intent {
                println!("Intent:  {}", i);
            }
            for line in msg.lines() {
                println!("    {}", line);
            }
            println!();
        }
        n += 1;
    }
    if n == 0 {
        println!("no changesets yet");
    }
    Ok(())
}

// changesets.

/// Create a changeset, recording which session produced it and why.
///
/// When an agent writes most of the code, the reason behind a change is part of the
/// change. A message says what happened; the session and intent keep it attached to the
/// conversation that produced it, so a reviewer can ask why instead of guessing from the
/// shape of the diff.
///
/// A snapshot covers the whole tree unless `paths` names some, which is what lets two writers share
/// one checkout without recording each other's files.
pub fn cmd_changeset_create_paths(
    message: &str,
    author: &str,
    session: Option<&str>,
    intent: Option<&str>,
    paths: &[String],
) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;

    match store::with_retry(|| repo::create_changeset_scoped(&conn, &root, message, author, paths))?
    {
        None => {
            println!(
                "nothing to snapshot: working tree matches branch {}",
                branch
            );
            Ok(())
        }
        Some(cs) => {
            // Recorded after the row exists, because the id comes from the snapshot.
            if session.is_some() || intent.is_some() {
                conn.execute(
                    "UPDATE changesets SET session = ?1, intent = ?2 WHERE id = ?3",
                    rusqlite::params![session, intent, cs.id],
                )?;
            }
            println!(
                "created changeset {} ({} file(s), branch {})",
                cs.id,
                cs.files.len(),
                branch
            );
            for (rel, old, _) in &cs.files {
                println!("  {} {}", if old.is_none() { "A" } else { "M" }, rel);
            }
            for rel in &cs.deleted {
                println!("  D {}", rel);
            }
            if !cs.skipped.is_empty() {
                println!("\nskipped {} binary file(s):", cs.skipped.len());
                for f in cs.skipped.iter().take(10) {
                    println!("  ? {}", f);
                }
            }
            println!("\nnext: gesta changeset commit {}", cs.id);
            Ok(())
        }
    }
}

/// Scan the content a changeset is about to store, and refuse if it looks like a credential.
///
/// At commit time, on the bytes that are about to become the record, because that is the last
/// moment where refusing costs nothing. After the commit the secret is in the history, and removing
/// it means rewriting a record other people may already have.
///
/// A finding is refused rather than warned about by default. A warning in a stream of output is a
/// warning nobody reads, and the alternative to a refused commit is a command that names the exact
/// entry to add to the allowlist, which takes one line.
#[cfg(feature = "regex")]
fn refuse_secrets(conn: &rusqlite::Connection, root: &std::path::Path, id: &str) -> ZResult<()> {
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

    let mut scanner = crate::leak::Scanner::default();
    scanner.load_allowlist(root);

    let mut found: Vec<crate::leak::Finding> = Vec::new();
    for path in &paths {
        if !crate::leak::worth_scanning(path) {
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

#[cfg(not(feature = "regex"))]
fn refuse_secrets(_conn: &rusqlite::Connection, _root: &std::path::Path, _id: &str) -> ZResult<()> {
    Ok(())
}

pub fn cmd_changeset_commit(id: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // Before the hook and before anything is written: a refusal here leaves the repository exactly
    // as it was, which is the whole point of checking at this moment.
    refuse_secrets(&conn, &root, id)?;
    // The hook runs before anything is written, so a refusal leaves the
    // repository exactly as it was. A hook that ran afterwards could only report
    // a commit that already happened.
    crate::hooks::run(&root, "pre-commit", &["commit", id])?;
    let n = store::with_retry(|| repo::commit_one(&conn, id))?;
    if n > 0 {
        // A post-commit failure is reported but does not undo the commit: the
        // commit is durable, and rolling it back here would leave the store and
        // the message disagreeing. The error still propagates so a caller knows.
        crate::hooks::run(&root, "post-commit", &["commit", id])?;
    }
    if n == 0 {
        println!("changeset {} already committed", id);
    } else {
        let branch = current_branch(&conn)?;
        println!("committed {} on branch {} ({} file(s))", id, branch, n);
        // Recorded after the fact with the state it produced, because undo restores an
        // operation's own snapshot and a commit has no earlier hook here. The important
        // part is that `undo` and this agree on the meaning; see `ops::record`.
        ops::record(&conn, "commit", Some(id), None)?;
        // The audit record held only what the server decided about requests. A commit is the same
        // kind of fact, and an operator reading the log saw arrivals and nothing about what this
        // machine did with them.
        crate::audit::local::commit(&root, &branch, id, "local", n);
    }
    Ok(())
}

pub fn cmd_changeset_apply(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let cutoff: Option<i64> = conn
        .query_row(
            "SELECT created_at FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    let cutoff = cutoff
        .ok_or_else(|| crate::error::Error::not_found(format!("no such changeset: {}", id)))?;

    let ids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' AND created_at <= ?1
             ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if ids.is_empty() {
        println!("no pending changesets to apply at or before {}", id);
        return Ok(());
    }
    let mut total = 0usize;
    for cs in &ids {
        total += store::with_retry(|| repo::commit_one(&conn, cs))?;
    }
    ops::record(&conn, "changeset apply", Some(id), None)?;
    println!(
        "applied {} changeset(s) up to {} ({} file(s))",
        ids.len(),
        id,
        total
    );
    Ok(())
}

pub fn cmd_changeset_commit_all() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let ids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if ids.is_empty() {
        println!("no pending changesets");
        return Ok(());
    }
    let mut total = 0usize;
    for cs in &ids {
        total += store::with_retry(|| repo::commit_one(&conn, cs))?;
    }
    ops::record(
        &conn,
        "commit all",
        None,
        Some(&format!("{} changesets", ids.len())),
    )?;
    println!("committed {} changeset(s) ({} file(s))", ids.len(), total);
    Ok(())
}

pub fn cmd_changeset_diff(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let msg: Option<String> = conn
        .query_row(
            "SELECT message FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .optional()?;
    println!(
        "changeset {}: {}",
        id,
        msg.ok_or_else(|| crate::error::Error::not_found(format!("no such changeset: {}", id)))?
    );
    print_renames(&conn, id)?;

    for (path, removed, added) in repo::changeset_diff(&conn, id)? {
        println!("\n--- {}", path);
        for l in &removed {
            println!("-{}", l);
        }
        for l in &added {
            println!("+{}", l);
        }
    }
    Ok(())
}

pub fn cmd_changeset_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let all: Vec<(String, String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at FROM changesets
             WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if all.is_empty() {
        println!("no pending changesets");
        return Ok(());
    }
    for (id, msg, author, ts) in all {
        println!("{:<14} {} {}  {}", id, fmt_ts(ts), author, msg);
    }
    Ok(())
}

// blame / history / show / checkout / restore.

pub fn cmd_revert(id: &str, reason: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let revert_id =
        store::with_retry(|| history::revert_changeset(&conn, &root, id, author, reason))?;
    ops::record(&conn, "revert", Some(&revert_id), Some(id))?;
    println!("reverted {} as {}", id, revert_id);
    println!("\nThe revert is a normal changeset: `gesta log` shows both, and");
    println!("reverting the revert restores the change.");
    Ok(())
}

/// Revisions whose lines should be attributed to whatever came before them.
///
/// The file lives at the repository root so a project can commit it and every clone inherits the
/// same answer, which is the point: a blame that depends on local configuration gives two people
/// two different histories.
const IGNORE_REVS_FILE: &str = ".gesta-blame-ignore-revs";

fn parse_rev_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        // A comment is how a project explains why a revision is on the list, and a list nobody
        // can explain stops being maintained.
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

fn read_ignore_revs(root: &Path) -> Vec<String> {
    match std::fs::read_to_string(root.join(IGNORE_REVS_FILE)) {
        Ok(text) => parse_rev_list(&text),
        Err(_) => Vec::new(),
    }
}

/// The changeset that should carry a line, walking back past revisions the project chose to skip.
///
/// `blame` holds one row per line, keyed by path and line number, so the skipped revision is
/// already the only answer stored. Walking back means asking which changeset last had that line
/// before the ignored one, which is a question `changeset_files` can answer: the ignored changeset
/// recorded both the file before and after, and its parent recorded the content it started from.
fn blame_through(
    conn: &Connection,
    path: &str,
    line_no: i64,
    ignored: &[String],
) -> ZResult<Option<(String, String, Option<String>)>> {
    let current: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT b.changeset_id, c.author, c.intent
             FROM blame_ranges b LEFT JOIN changesets c ON c.id = b.changeset_id
             WHERE b.path = ?1 AND b.start_line <= ?2 AND b.end_line >= ?2",
            rusqlite::params![path, line_no],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let cs = match &current {
        Some(found) => found.0.clone(),
        None => return Ok(None),
    };
    if !ignored.iter().any(|i| i == &cs) {
        return Ok(current);
    }
    // The line's content did not change in the ignored revision, so blame it on the newest
    // changeset before it that touched this file at all. That is an approximation of what git
    // computes by re-running the diff, and it is the honest one to report: the line came from
    // somewhere earlier, and this names where.
    let earlier: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT cf.changeset_id, c.author, c.intent
             FROM changeset_files cf JOIN changesets c ON c.id = cf.changeset_id
             WHERE cf.path = ?1 AND cf.changeset_id != ?2
               AND c.created_at <= (SELECT created_at FROM changesets WHERE id = ?2)
             ORDER BY c.created_at DESC, c.rowid DESC LIMIT 1",
            rusqlite::params![path, cs],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(earlier.or(current))
}

pub fn cmd_blame(file: &str, ignore: &[String]) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let rel = normalise(file);

    // Following a revision through a rewrite is what keeps blame useful. A tree-wide reformat or a
    // rename of every symbol makes every line point at one changeset, which is true and useless:
    // the reader wants the change that last touched the line in substance. Git settled on a file
    // at the repository root listing those revisions, read without being asked, and the same
    // convention here means a project does not have to teach every tool about its own list.
    let ignore = if ignore.is_empty() {
        read_ignore_revs(&root)
    } else {
        ignore
            .iter()
            .flat_map(|v| {
                // A caller may name a file of revisions, or a revision directly.
                match std::fs::read_to_string(root.join(v)) {
                    Ok(text) => parse_rev_list(&text),
                    Err(_) => vec![v.clone()],
                }
            })
            .collect()
    };

    let content = match worktree::read_text(&root.join(&rel)) {
        Some(c) => c,
        None => {
            let branch = current_branch(&conn)?;
            let h = store::latest_hash(&conn, &rel, &branch)?
                .ok_or_else(|| bad(format!("file not found: {}", rel)))?;
            get_object(&conn, &h)?
        }
    };

    // A blame row answers who wrote a line. With provenance it can also answer which
    // session wrote it and why, which is the question that actually comes up when an
    // agent produced most of the file.
    let rows: Vec<(i64, String, String, Option<String>)> = {
        // Expand the ranges into one entry per line, because the caller prints a line at a time.
        // The expansion happens here rather than in the store: a range is the cheapest way to
        // keep the answer, and a list of lines is the cheapest way to read it.
        let mut stmt = conn.prepare(
            "SELECT b.start_line, b.end_line, b.changeset_id, c.author, c.intent
             FROM blame_ranges b LEFT JOIN changesets c ON c.id = b.changeset_id
             WHERE b.path = ?1 ORDER BY b.start_line",
        )?;
        let ranges = stmt
            .query_map(rusqlite::params![rel], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (start, end, cs, author, intent) in ranges {
            for line in start..=end {
                out.push((
                    line,
                    cs.clone(),
                    author.clone().unwrap_or_default(),
                    intent.clone(),
                ));
            }
        }
        out
    };

    if rows.is_empty() {
        println!("no blame data for {} (never committed)", rel);
        return Ok(());
    }
    let map: std::collections::HashMap<i64, (String, String, Option<String>)> = rows
        .into_iter()
        .map(|(n, c, a, i)| (n, (c, a, i)))
        .collect();
    // Report how many lines the ignore list moved, because a silently different answer is how a
    // reader loses trust in the attribution without knowing why it changed.
    let mut skipped = 0usize;

    let mut unattributed = 0usize;
    let mut intents: Vec<String> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let n = (i + 1) as i64;
        let looked = match map.get(&n) {
            Some(found) if ignore.iter().any(|i| i == &found.0) => {
                skipped += 1;
                blame_through(&conn, &rel, n, &ignore)?
            }
            Some(found) => Some(found.clone()),
            None => blame_through(&conn, &rel, n, &ignore)?,
        };
        match looked.as_ref() {
            Some((cs, author, intent)) => {
                println!("{:<14} {:<10} {:>4} | {}", cs, author, n, line);
                // Collected rather than printed per line, because a reason repeated on
                // every line of a block is noise. It is listed once, at the end.
                if let Some(it) = intent {
                    let entry = format!("{}: {}", &cs[..cs.len().min(14)], it);
                    if !intents.contains(&entry) {
                        intents.push(entry);
                    }
                }
            }
            None => {
                unattributed += 1;
                println!("{:<14} {:<10} {:>4} | {}", "?", "?", n, line)
            }
        }
    }
    if skipped > 0 {
        println!(
            "\n{} line(s) would point at an ignored revision; attributed to the change before it",
            skipped
        );
    }
    if !intents.is_empty() {
        println!("\nwhy these lines exist:");
        for i in &intents {
            println!("  {}", i);
        }
    }
    if unattributed > 0 {
        eprintln!(
            "warning: {} line(s) have no attributed changeset",
            unattributed
        );
    }
    Ok(())
}

pub fn cmd_history(file: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let rel = normalise(file);

    // Follow renames backwards so a file that was moved still shows the history
    // it earned under its old name.
    let chain = history::rename_history(&conn, &rel, 10)?;

    let mut all: Vec<(String, i64, String, i64, String, String)> = Vec::new();
    for name in &chain {
        // The second column is the changeset that wrote this version, not the version's content
        // hash. A reader copies that column into `show`, and a content hash is a name no command
        // resolves: it looked like the commit and was the file's own hash.
        let mut stmt = conn.prepare(
            "SELECT fv.path, fv.version, cs.id, fv.created_at, cs.message, cs.author
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        for r in rows {
            all.push(r?);
        }
    }

    if all.is_empty() {
        println!("no history for {}", rel);
        return Ok(());
    }
    if chain.len() > 1 {
        println!("following renames: {}", chain.join(" <- "));
    }
    for (name, ver, hash, ts, msg, author) in all {
        println!(
            "v{:<3} {} {} {:<10} {:<20} {}",
            ver,
            &hash[..8.min(hash.len())],
            fmt_ts(ts),
            author,
            name,
            msg
        );
    }
    Ok(())
}

/// Push this repository's branches to a git remote.
pub fn cmd_git_push(remote: &str, branch: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = crate::git_bridge::push_to_git_remote(&conn, branch, remote)?;
    println!("pushed {} commit(s) to {}", out.commits, remote);
    if !out.branches.is_empty() {
        println!("branches: {}", out.branches.join(", "));
    }
    Ok(())
}

/// Fetch from a git remote and import what arrived.
pub fn cmd_git_pull(remote: &str, prefix: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let got = crate::git_bridge::pull_from_git_remote(&conn, remote, prefix)?;
    println!(
        "imported {} commit(s), {} already present",
        got.commits, got.skipped
    );
    if !got.branches.is_empty() {
        println!("branches: {}", got.branches.join(", "));
    }
    Ok(())
}

/// List the files a revision holds, the way `git ls-tree -r --name-only` does.
///
/// An agent that has just been handed a repository needs this before it can name a path:
/// without it the only way to discover the tree is to read every file the log mentions or to
/// guess. Printing one path per line is also what makes the output pipeable.
pub fn cmd_ls(rev: Option<&str>, prefix: Option<&str>, long: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    for (path, hash) in ls_entries(&conn, rev, prefix)? {
        if long {
            println!("{} {}", &hash[..hash.len().min(12)], path);
        } else {
            println!("{}", path);
        }
    }
    Ok(())
}

/// The same list as one JSON object, so a caller does not parse lines.
pub fn cmd_ls_json(rev: Option<&str>, prefix: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let entries = ls_entries(&conn, rev, prefix)?;
    emit(crate::json::Json::obj(vec![
        (
            "files",
            crate::json::Json::arr(
                entries
                    .iter()
                    .map(|(p, h)| {
                        crate::json::Json::obj(vec![
                            ("path", crate::json::Json::str(p.clone())),
                            ("hash", crate::json::Json::str(h.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("count", crate::json::Json::int(entries.len() as i64)),
    ]));
    Ok(())
}

/// Resolve the revision, then hand back its tree sorted by path.
fn ls_entries(
    conn: &rusqlite::Connection,
    rev: Option<&str>,
    prefix: Option<&str>,
) -> ZResult<Vec<(String, String)>> {
    let head = match rev {
        Some(r) => crate::store::resolve_rev(conn, r)?
            .ok_or_else(|| {
                crate::error::Error::not_found(format!(
                    "no such revision: {}. A revision may be HEAD, a branch, a tag, a changeset id, or an unambiguous prefix of one.",
                    r
                ))
            })?,
        None => {
            let branch = current_branch(conn)?;
            crate::store::branch_head(conn, &branch)?.ok_or_else(|| {
                Box::new(crate::error::Error::new(
                    crate::error::Code::NotFound,
                    format!("branch {} has no changesets yet", branch),
                )) as Box<dyn std::error::Error>
            })?
        }
    };
    let mut tree: Vec<(String, String)> = crate::store::tree_at(conn, &head)?.into_iter().collect();
    tree.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(p) = prefix {
        let p = crate::normalise(p);
        tree.retain(|(path, _)| {
            path == &p || path.starts_with(&format!("{}/", p.trim_end_matches('/')))
        });
    }
    Ok(tree)
}

/// Create a signing key and print the public half.
pub fn cmd_key_generate(path: Option<&str>) -> ZResult<()> {
    let path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::sign::default_key_path(),
    };
    let key = crate::sign::SigningKey::generate(&path)?;
    // Recorded here as well as on rotation: a repository that never rotates still has to be able to
    // answer "which keys were ours", and a record that starts at the first rotation answers for half
    // the history.
    if let Ok((root, _)) = open_repo() {
        crate::keyring::record_active(&root, &key.public_hex(), &key.path, crate::now())?;
    }
    println!("created a signing key: {}", key.path.display());
    println!("public key: {}", key.public_hex());
    println!();
    println!("Give that public key to anyone who should verify your changesets.");
    Ok(())
}

/// Replace the active signing key, archiving the one in use.
///
/// Nothing is re-signed. A signature names the public key it was made with, so an old changeset keeps
/// verifying against the old key for as long as that public key is known. The archive exists for the
/// case a person cannot reconstruct: a signature is disputed and the private half has to be found.
pub fn cmd_key_rotate(path: Option<&str>, dry_run: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let target = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::sign::default_key_path(),
    };

    // What is active now, from the record rather than from the file: a key made with `--path` lives
    // somewhere else, and rotating the default path would leave the real key in place and sign with
    // the wrong one for as long as nobody looked.
    let current = crate::keyring::active(&root)?;
    let current_path = current
        .as_ref()
        .map(|r| std::path::PathBuf::from(&r.path))
        .unwrap_or_else(|| target.clone());

    if dry_run {
        println!("would archive: {}", current_path.display());
        println!("would create:  {}", target.display());
        if let Some(r) = &current {
            println!(
                "the key in use is {}",
                &r.public_hex[..16.min(r.public_hex.len())]
            );
        } else {
            println!("no key is recorded as active");
        }
        return Ok(());
    }

    // The old key moves aside before the new one is made, so a crash between the two steps leaves an
    // archived key and no active one rather than two keys claiming the same filename.
    let mut archived = None;
    if current_path.exists() {
        let public = match crate::sign::SigningKey::load(&current_path) {
            Ok(k) => k.public_hex(),
            Err(_) => crate::keyring::active(&root)?
                .map(|r| r.public_hex)
                .unwrap_or_default(),
        };
        if !public.is_empty() {
            archived = Some(crate::keyring::archive(&current_path, &public)?);
        }
    }

    let key = crate::sign::SigningKey::generate(&target)?;
    crate::keyring::record_active(&root, &key.public_hex(), &key.path, crate::now())?;

    crate::audit::local::key_rotated(
        &root,
        "local",
        &key.public_hex(),
        archived
            .as_ref()
            .map(|p| p.display().to_string())
            .as_deref(),
    );
    if let Some(a) = archived {
        println!("archived the old key to {}", a.display());
    }
    println!("created a new signing key: {}", key.path.display());
    println!("public key: {}", key.public_hex());
    println!();
    println!("Changesets signed with the old key still verify: a signature names the key it was made with.");
    println!("Give the new public key to anyone who should verify your changesets.");
    Ok(())
}

/// Show the keys this repository has used.
pub fn cmd_key_list() -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let records = crate::keyring::read_records(&root)?;
    if records.is_empty() {
        println!("no signing keys recorded for this repository");
        println!("create one with: gesta key generate");
        return Ok(());
    }
    for r in records {
        let state = match r.retired_at {
            None => "active".to_string(),
            // A unix second, printed as one. There is no date formatter in this tree and a
            // rotation is read once, so a number that can be compared is enough.
            Some(t) => format!("retired at {} (unix)", t),
        };
        let present = if std::path::Path::new(&r.path).exists() {
            ""
        } else {
            "  (file is gone)"
        };
        println!(
            "{}  {}  {}{}",
            &r.public_hex[..16.min(r.public_hex.len())],
            state,
            r.path,
            present
        );
    }
    Ok(())
}

/// Sign a pending changeset with the local key.
pub fn cmd_sign(id: &str, key_path: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (message, author, parent, status): (String, String, Option<String>, String) = conn
        .query_row(
            "SELECT message, author, parent, status FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| crate::error::Error::not_found(format!("no such changeset: {}", id)))?;
    if status == "committed" {
        return Err(bad(format!(
            "{} is already committed: signing after the fact signs a different history than the one anybody else has",
            id
        )));
    }
    let path = match key_path {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::sign::default_key_path(),
    };
    let key = crate::sign::SigningKey::load(&path)?;
    let signature = key.sign_changeset(id, &message, &author, parent.as_deref());
    conn.execute(
        "UPDATE changesets SET signature = ?2, signer = ?3 WHERE id = ?1",
        rusqlite::params![id, signature, key.public_hex()],
    )?;
    println!("signed {} with {}", id, key.public_hex());
    Ok(())
}

/// Check a changeset's signature.
pub fn cmd_verify(id: Option<&str>, key_hex: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let id = match id {
        Some(i) => i.to_string(),
        None => {
            let branch = current_branch(&conn)?;
            branch_head(&conn, &branch)?.ok_or_else(|| {
                Box::new(crate::error::Error::new(
                    crate::error::Code::NotFound,
                    "the branch has no changesets to verify",
                )) as Box<dyn std::error::Error>
            })?
        }
    };
    /// What a verification needs: the fields the signature covered, plus the signature itself.
    type SignedRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<SignedRow> = conn
        .query_row(
            "SELECT message, author, parent, signature, signer FROM changesets WHERE id = ?1",
            rusqlite::params![&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (message, author, parent, signature, signer) =
        row.ok_or_else(|| crate::error::Error::not_found(format!("no such changeset: {}", id)))?;
    let signature =
        signature.ok_or_else(|| bad(format!("{} carries no signature: nothing to verify", id)))?;
    let public = match key_hex {
        Some(k) => k.to_string(),
        None => signer.ok_or_else(|| {
            bad(format!(
                "{} records no signing key, so pass --key with the public key to check against",
                id
            ))
        })?,
    };
    crate::sign::verify(
        &public,
        &id,
        &message,
        &author,
        parent.as_deref(),
        &signature,
    )?;
    println!("{} verifies against {}", id, public);
    // The maths checking out answers "was this signed by that key", not "is that key ours".
    // Any signature verifies against its own public half, so a key this repository has never
    // used is worth naming: it means the changeset arrived from somewhere else.
    if !crate::keyring::was_ours(&root, &public)? {
        if let Some(a) = crate::keyring::active(&root)? {
            if a.public_hex != public {
                println!(
                    "note: {} is not a key this repository has recorded; the active one is {}",
                    &public[..16.min(public.len())],
                    &a.public_hex[..16.min(a.public_hex.len())]
                );
            }
        }
    }
    Ok(())
}

/// Read a git repository into this one.
pub fn cmd_git_import(path: &str, prefix: Option<&str>) -> ZResult<()> {
    // The source is checked first. Opening the destination first meant a mistyped source path
    // reported "not a repository: / and no directory above it holds one", which names the directory
    // the tool was run from rather than the argument the person got wrong.
    let git_root = Path::new(path);
    crate::gitobj::Repo::open(git_root)?;
    let (_root, conn) = open_repo()?;
    let got = crate::git_bridge::import(&conn, git_root, prefix)?;
    println!(
        "imported {} commit(s) as changesets, {} path change(s), {} already present",
        got.commits, got.files, got.skipped
    );
    if !got.branches.is_empty() {
        println!("branches: {}", got.branches.join(", "));
    }
    Ok(())
}

/// Write this repository out as a git repository.
pub fn cmd_git_export(path: &str, branch: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = crate::git_bridge::export(&conn, Path::new(path), branch)?;
    println!("exported {} commit(s)", out.commits);
    if !out.branches.is_empty() {
        println!("branches: {}", out.branches.join(", "));
    }
    if !out.root.is_empty() {
        println!("head: {}", out.root);
    }
    Ok(())
}

/// Do the safe half of what `brief` reports.
pub fn cmd_brief_fix(json_out: bool, stash: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let f = crate::brief::fix_with(
        &conn,
        &root,
        crate::brief::Options {
            stash_uncommitted: stash,
        },
    )?;
    if json_out {
        emit(crate::brief::fixed_to_json(&f));
        return Ok(());
    }
    for id in &f.committed {
        println!("committed {}", id);
    }
    for id in &f.dropped {
        println!("dropped {} (the disk had moved past it)", id);
    }
    for (id, files) in &f.stashed {
        println!("stashed {} file(s) as stash {}", files, id);
    }
    for (what, why) in &f.skipped {
        println!("left alone: {} -- {}", what, why);
    }
    if f.committed.is_empty()
        && f.dropped.is_empty()
        && f.stashed.is_empty()
        && f.skipped.is_empty()
    {
        println!("nothing to fix");
    }
    Ok(())
}

/// Everything a session needs before its first edit, printed as a few short lines.
pub fn cmd_brief() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let b = crate::brief::brief(&conn, &root)?;
    println!("branch:   {}", b.branch);
    println!("head:     {}", b.head.as_deref().unwrap_or("(empty)"));
    println!("next:     {}", b.next.as_str());
    if b.changed > 0 {
        println!("changed:  {} file(s) differ from the head", b.changed);
    }
    if !b.pending.is_empty() {
        println!("pending:  {}", b.pending.join(", "));
    }
    if !b.stale.is_empty() {
        println!(
            "stale:    {} (the disk moved on; discard before committing)",
            b.stale.join(", ")
        );
    }
    if b.rebase_pending > 0 {
        println!("rebase:   {} changeset(s) left to replay", b.rebase_pending);
    }
    if b.stashes > 0 {
        println!("stashes:  {}", b.stashes);
    }
    if !b.conflicts.is_empty() {
        println!("conflict: {}", b.conflicts.join(", "));
    }
    let advice = b.next.advice();
    if !advice.is_empty() {
        println!("\n{}", advice);
    }
    Ok(())
}

pub fn cmd_brief_json() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let b = crate::brief::brief(&conn, &root)?;
    emit(crate::brief::to_json(&b));
    Ok(())
}

/// How a search was asked for, so the two entry points share one shape.
///
/// Gathered into a struct rather than passed as eight arguments: the call sites read as
/// `Search { pattern: "x", history: true, ..Default::default() }`, and a new flag does not
/// change every signature that carries it.
#[derive(Default, Clone)]
pub struct Search {
    pub pattern: String,
    pub history: bool,
    pub ignore_case: bool,
    pub word: bool,
    pub regex: bool,
    pub path: Option<String>,
    pub limit: i64,
    pub files_with_matches: bool,
    /// Cut each matching line to this many characters. Zero keeps lines whole.
    pub max_line: usize,
}

/// Search the working tree or the history.
pub fn cmd_grep(s: Search) -> ZResult<()> {
    let hits = grep_hits(&s)?;
    if s.files_with_matches {
        let mut seen = std::collections::BTreeSet::new();
        for h in &hits {
            if seen.insert(h.path.clone()) {
                println!("{}", h.path);
            }
        }
        if hits.is_empty() {
            return Err(bad(no_match(&s.pattern, s.history)));
        }
        return Ok(());
    }
    // A single minified line can be thousands of characters, and a person reading a terminal and
    // an agent reading a transcript pay for all of it. Cutting is opt-in, because a caller that
    // wants the whole line has no other place to get it, and a cut line says how long it was.
    let mut cut = 0usize;
    let clip = |text: &str| -> String {
        if s.max_line == 0 || text.chars().count() <= s.max_line {
            return text.to_string();
        }
        let mut out: String = text.chars().take(s.max_line).collect();
        out.push_str(&format!(" [{} chars, cut]", text.chars().count()));
        out
    };
    for h in &hits {
        if s.max_line > 0 && h.text.chars().count() > s.max_line {
            cut += 1;
        }
        if s.history {
            let short = &h.rev[..h.rev.len().min(14)];
            println!("{}:{}:{}: {}", short, h.path, h.line, clip(&h.text));
        } else {
            println!("{}:{}: {}", h.path, h.line, clip(&h.text));
        }
    }
    if cut > 0 {
        // The count and the way back, because a reader who is told only that something was
        // shortened cannot see what it said.
        eprintln!(
            "note: {} line(s) were cut to {} characters; run without --max-line to see them whole",
            cut, s.max_line
        );
    }
    if hits.is_empty() {
        return Err(bad(no_match(&s.pattern, s.history)));
    }
    Ok(())
}

/// The machine-readable form, which is what an agent should use: it decides what to do with a
/// list of locations instead of parsing a terminal's layout.
pub fn cmd_grep_json(s: Search) -> ZResult<()> {
    let hits = grep_hits(&s)?;
    let rows: Vec<crate::json::Json> = hits
        .iter()
        .map(|h| {
            crate::json::Json::obj(vec![
                ("path", crate::json::Json::str(h.path.clone())),
                ("line", crate::json::Json::int(h.line as i64)),
                ("rev", crate::json::Json::str(h.rev.clone())),
                ("text", crate::json::Json::str(h.text.clone())),
            ])
        })
        .collect();
    emit(crate::json::Json::obj(vec![
        ("pattern", crate::json::Json::str(s.pattern.clone())),
        ("history", crate::json::Json::Bool(s.history)),
        ("count", crate::json::Json::int(hits.len() as i64)),
        ("hits", crate::json::Json::arr(rows)),
    ]));
    Ok(())
}

/// Build the search from the flags, refusing `--regex` in a build that does not have it.
fn build_pattern(
    needle: &str,
    ignore_case: bool,
    word: bool,
    regex: bool,
) -> ZResult<crate::grep::Pattern> {
    #[cfg(feature = "regex")]
    if regex {
        return crate::grep::Pattern::regex(needle, ignore_case, word);
    }
    #[cfg(not(feature = "regex"))]
    if regex {
        return Err(bad(
            "this build has no regular expression support: rebuild with --features regex, or search for literal text",
        ));
    }
    let _ = regex;
    crate::grep::Pattern::new(needle, ignore_case, word)
}

fn no_match(needle: &str, history: bool) -> String {
    format!(
        "no match for {} in {}",
        needle,
        if history {
            "the recorded history"
        } else {
            "the working tree"
        }
    )
}

fn grep_hits(s: &Search) -> ZResult<Vec<crate::grep::Hit>> {
    let (root, conn) = open_repo()?;
    let pattern = build_pattern(&s.pattern, s.ignore_case, s.word, s.regex)?;
    let filter = s.path.as_deref().map(normalise);
    if s.history {
        crate::grep::search_history(&conn, &pattern, filter.as_deref(), s.limit)
    } else {
        crate::grep::search_worktree(&conn, &root, &pattern, filter.as_deref())
    }
}

pub fn cmd_show(spec: &str, json: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (rev, path, named_path) = match spec.split_once(':') {
        Some((r, p)) => (r.to_string(), normalise(p), true),
        None => (current_branch(&conn)?, normalise(spec), false),
    };
    // `show v2` with no path: a version label belongs to a file, so this is a question with no answer
    // rather than a name that does not exist. Checked against `spec`, because without a colon the
    // revision is the current branch and `spec` is the path.
    if !named_path {
        if let Some(rest) = spec.strip_prefix('v') {
            if rest.parse::<i64>().is_ok() {
                return Err(crate::error::Error::new(
                    crate::error::Code::Usage,
                    format!(
                        "{} is a per-file version, so it needs a path: `gesta show {}:<file>`",
                        spec, spec
                    ),
                )
                .into());
            }
        }
    }
    // A `vN` label names a version of one file. Here the file is named too, so it resolves; the
    // ambiguous form is refused by `resolve_rev` with the paths it could mean.
    let cs = match store::resolve_version_for_path(&conn, &rev, &path)? {
        Some(id) => id,
        None => match store::resolve_rev(&conn, &rev) {
            Ok(Some(id)) => id,
            // The resolver already refuses an ambiguous `vN` with the paths it could mean. Anything
            // else is a name that matches nothing, and the message says which forms exist.
            Err(e) => return Err(e),
            Ok(None) => {
                return Err(crate::error::Error::not_found(format!(
                    "no such branch, tag or changeset: {}. A revision may be HEAD, a branch, a tag, a changeset id, an unambiguous prefix of one, or vN with a path.",
                    rev
                ))
                .into())
            }
        },
    };
    let hash = hash_at_point(&conn, &path, &cs)?.ok_or_else(|| {
        crate::error::Error::new(
            crate::error::Code::PathNotFound,
            format!("{} does not exist at {}", path, rev),
        )
    })?;
    // Bytes, not text. `show` on a binary file failed with a message about UTF-8 because it went
    // through the text reader, and a large binary is exactly the file somebody shows.
    let bytes = crate::store::get_object_bytes(&conn, &hash)?;
    if json {
        // `--json` printed the file's bytes and called it JSON, so a caller piping into a parser got
        // the file's contents where an object should be and had no way to tell. A body that is not
        // UTF-8 cannot be a JSON string at all.
        let text = std::str::from_utf8(&bytes).ok();
        let mut fields = vec![
            ("path", crate::json::Json::str(path.clone())),
            ("rev", crate::json::Json::str(rev.clone())),
            ("changeset", crate::json::Json::str(cs.clone())),
            ("hash", crate::json::Json::str(hash.clone())),
            ("size", crate::json::Json::int(bytes.len() as i64)),
            ("binary", crate::json::Json::Bool(text.is_none())),
        ];
        match text {
            Some(t) => fields.push(("content", crate::json::Json::str(t))),
            None => fields.push((
                "content_base64",
                crate::json::Json::str(crate::remote::enc_base64(&bytes)),
            )),
        }
        emit_object(fields);
        return Ok(());
    }
    use std::io::Write;
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}

pub fn cmd_checkout(file: &str, changeset: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    if file == "." {
        let branch = current_branch(&conn)?;
        let (written, removed) = repo::restore_worktree(&conn, &root, &branch)?;
        println!(
            "restored {} file(s), removed {} stale file(s)",
            written, removed
        );
        return Ok(());
    }
    let rel = normalise(file);
    let bytes = repo::checkout_file(&conn, &root, &rel, changeset)?;
    println!("checked out {} ({} bytes)", rel, bytes);
    Ok(())
}

/// Write the whole store to one file.
///
/// Different from `restore`, which puts working-tree files back from a branch. This copies the
/// repository itself, including the parts no bundle carries: tags, blame ranges, every branch.
pub fn cmd_backup(dest: &str, json: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let path = std::path::PathBuf::from(dest);
    let report = crate::backup::backup(&conn, &path)?;
    if json {
        let doc = crate::json::Json::obj(vec![
            ("schema", crate::json::Json::str("gesta@1")),
            (
                "path",
                crate::json::Json::str(report.path.display().to_string()),
            ),
            ("bytes", crate::json::Json::int(report.bytes as i64)),
            ("changesets", crate::json::Json::int(report.changesets)),
            ("objects", crate::json::Json::int(report.objects)),
            ("branches", crate::json::Json::int(report.branches)),
            ("tags", crate::json::Json::int(report.tags)),
        ]);
        println!("{}", doc.render());
        return Ok(());
    }
    println!(
        "backed up {} change(s), {} object(s), {} branch(es), {} tag(s) to {} ({} bytes)",
        report.changesets,
        report.objects,
        report.branches,
        report.tags,
        report.path.display(),
        report.bytes
    );
    Ok(())
}

/// Check a backup file. Reads only the file, so it is safe to run against a repository in use.
pub fn cmd_verify_backup(file: &str) -> ZResult<()> {
    let path = std::path::PathBuf::from(file);
    let c = crate::backup::verify(&path)?;
    println!(
        "{} is readable: {} change(s), {} object(s), {} branch(es), {} tag(s), {} version(s), {} blame range(s)",
        path.display(),
        c.changesets,
        c.objects,
        c.branches,
        c.tags,
        c.file_versions,
        c.blame_ranges
    );
    Ok(())
}

/// Replace this repository with a backup.
///
/// The destination is the repository this command is run inside, and the old store is kept as
/// `<db>.before-restore` until the new one has opened, so a failure puts it back.
pub fn cmd_restore_repo(file: &str, yes: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let path = std::path::PathBuf::from(file);
    let c = crate::backup::verify(&path)?;
    let here = crate::backup::contents(&conn)?;

    if !yes {
        // A restore replaces the store. Asking is the difference between recovering from a backup
        // and destroying a repository because the wrong file was named.
        return Err(Box::new(crate::error::Error::new(
            crate::error::Code::Usage,
            format!(
                "this replaces {} change(s) here with {} from {}. Pass --yes to do it",
                here.changesets,
                c.changesets,
                path.display()
            ),
        )));
    }

    drop(conn);
    let restored = crate::backup::restore(&root, &path)?;
    println!(
        "restored {} change(s), {} object(s), {} branch(es), {} tag(s) from {}",
        restored.changesets,
        restored.objects,
        restored.branches,
        restored.tags,
        path.display()
    );
    Ok(())
}

pub fn cmd_restore(file: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    if file == "." {
        let (written, removed) = repo::restore_worktree(&conn, &root, &branch)?;
        println!(
            "restored {} file(s), removed {} stale file(s)",
            written, removed
        );
        return Ok(());
    }
    let rel = normalise(file);
    let bytes = repo::checkout_file(&conn, &root, &rel, None)?;
    println!("restored {} from branch {} ({} bytes)", rel, branch, bytes);
    Ok(())
}

pub fn cmd_undo() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let msg = ops::undo(&conn, &root)?;
    println!("{}", msg);
    Ok(())
}

/// `gesta redo`: put back the most recently undone operation.
pub fn cmd_redo() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let msg = ops::redo(&conn, &root)?;
    println!("{}", msg);
    Ok(())
}

pub fn cmd_ops(limit: usize) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let ops = ops::list(&conn, limit)?;
    if ops.is_empty() {
        println!("no operations recorded");
        return Ok(());
    }
    for o in ops {
        println!(
            "{:>5}  {}  {:<16} {}{}",
            o.id,
            fmt_ts(o.created_at),
            o.kind,
            o.target.unwrap_or_default(),
            o.detail.map(|d| format!(" ({})", d)).unwrap_or_default()
        );
    }
    Ok(())
}

pub fn cmd_tag(name: Option<&str>, changeset: Option<&str>, list: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if list {
        let tags: Vec<(String, String, i64)> = {
            let mut stmt =
                conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if tags.is_empty() {
            println!("no tags");
        }
        for (n, cs, ts) in tags {
            println!("{:<20} {} {}", n, cs, fmt_ts(ts));
        }
        return Ok(());
    }
    let name =
        name.ok_or_else(|| bad("a tag needs a name: `gesta tag <name> [--changeset REV]`"))?;

    // Whatever was named has to be turned into a changeset id before it is stored. Storing the
    // name is what let `tag v2 main` succeed while pointing at a string that is not a changeset:
    // the tag exists, resolves to nothing, and fails only when somebody reads through it.
    let target = match changeset {
        Some(c) => crate::store::resolve_rev(&conn, c)?.ok_or_else(|| {
            bad(format!(
                "no such branch, tag or changeset: {}; a tag can point at a branch, a changeset \
                 or another tag, but it has to point at something",
                c
            ))
        })?,
        None => {
            let branch = current_branch(&conn)?;
            branch_head(&conn, &branch)?.ok_or_else(|| bad("branch has no changesets to tag"))?
        }
    };
    conn.execute(
        "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, NULL, ?3)
         ON CONFLICT(name) DO UPDATE SET changeset_id = excluded.changeset_id",
        rusqlite::params![name, target, now()],
    )?;
    ops::record(&conn, "tag", Some(name), None)?;
    println!("tagged {} at {}", name, target);
    Ok(())
}

// branches.

pub fn cmd_branch_create(name: &str, from: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM branches WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        println!("branch {} already exists", name);
        return Ok(());
    }
    let head = branch_head(&conn, from)?;
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![name, head, now(), from],
    )?;
    // A new branch points at the same changeset, so it starts with the same
    // committed tree. Without this the branch reads as an empty tree until its
    // first commit, and `status` on it reports every file as new.
    crate::store::refresh_branch_tree(&conn, name)?;
    ops::record(&conn, "branch create", Some(name), Some(from))?;
    println!(
        "created branch {} from {} at {}",
        name,
        from,
        head.unwrap_or_else(|| "(empty)".into())
    );
    Ok(())
}

pub fn cmd_branch_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let cur = current_branch(&conn)?;
    let all: Vec<(String, Option<String>, i64)> = {
        let mut stmt =
            conn.prepare("SELECT name, head, created_at FROM branches ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head, ts) in all {
        let mark = if name == cur { "*" } else { " " };
        match head {
            Some(h) => println!("{} {:<16} head={} created={}", mark, name, h, fmt_ts(ts)),
            None => println!("{} {:<16} (empty)      created={}", mark, name, fmt_ts(ts)),
        }
    }
    Ok(())
}

pub fn cmd_branch_switch(name: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let target = branch_head(&conn, name)?;
    set_state(&conn, "current_branch", name)?;
    // Switching branches must make the working tree match the new head, or the
    // next status would report every file as modified.
    let (written, removed) = repo::restore_worktree(&conn, &root, name)?;
    ops::record(&conn, "branch switch", Some(name), None)?;
    println!(
        "switched to branch {} ({} file(s) materialised, {} removed, head {})",
        name,
        written,
        removed,
        target.unwrap_or_else(|| "(empty)".into())
    );
    Ok(())
}

pub fn cmd_branch_delete(name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let cur = current_branch(&conn)?;
    if name == cur {
        return Err(bad(format!(
            "cannot delete the branch you are on ({})",
            name
        )));
    }
    if branch_head(&conn, name).is_err() {
        return Err(bad(format!(
            "no such branch: {}{}",
            name,
            branch_names(&conn)
        )));
    }
    conn.execute(
        "DELETE FROM branches WHERE name = ?1",
        rusqlite::params![name],
    )?;
    // The derived tree goes with the branch. Leaving it behind would make a
    // branch created later under the same name start with a stale tree.
    conn.execute(
        "DELETE FROM branch_tree WHERE branch = ?1",
        rusqlite::params![name],
    )?;
    ops::record(&conn, "branch delete", Some(name), None)?;
    println!("deleted branch {}", name);
    Ok(())
}

/// Protect a branch, or stop protecting it.
///
/// The gate is a row on the branch plus an approval per changeset, and both live in the store, so a
/// clone carries them. A hook file would be simpler and would also be something an operator has to
/// install and keep installed.
pub fn cmd_branch_protect(name: &str, unset: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if crate::store::branch_head(&conn, name).is_err() {
        return Err(bad(format!(
            "no such branch: {}{}",
            name,
            branch_names(&conn)
        )));
    }
    if !crate::approval::set_protected(&conn, name, !unset)? {
        return Err(bad(format!("no such branch: {}", name)));
    }
    if unset {
        println!(
            "{} is no longer protected; a push to it needs no approval",
            name
        );
    } else {
        println!(
            "{} is protected: a push to it now needs a sign-off from a credential other than the one pushing",
            name
        );
        println!("sign off a changeset with: gesta changeset approve <id> --by <reviewer>");
    }
    Ok(())
}

/// Sign off a changeset.
pub fn cmd_approve(id: &str, by: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch: String = conn
        .query_row(
            "SELECT COALESCE(branch, 'main') FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .map_err(|_| bad(format!("no such changeset: {}", id)))?;
    crate::approval::approve(&conn, id, &branch, by)?;
    // A sign-off is the event a protected branch turns on, so it is exactly as worth a line as a
    // refused request.
    crate::audit::local::approve(&root, id, by, &branch);
    let signers = crate::approval::approvers(&conn, id)?;
    println!(
        "{} approved by {} (approvals: {})",
        id,
        by,
        signers.join(", ")
    );
    if crate::approval::is_protected(&conn, &branch)? && signers.len() == 1 {
        // Worth saying here rather than at the push: a single approval is enough only if the pusher
        // is a different credential, and that is the thing people get wrong.
        println!(
            "note: {} is protected, so this approval counts only for a push from a different credential than {}",
            branch, by
        );
    }
    Ok(())
}

/// Print the changeset that holds a git commit.
///
/// Matched on a prefix rather than the whole id: a person reading `git log` has the short form, and
/// asking them to fetch the long one is the reason a lookup gets skipped.
pub fn cmd_changeset_for_commit(commit: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id, COALESCE(git_commit, '') FROM changesets
             WHERE git_commit IS NOT NULL AND git_commit LIKE ?1
             ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![format!("{}%", commit)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match row {
        Some((id, full)) => println!("{} {}", id, full),
        None => {
            return Err(bad(format!(
                "no changeset holds a commit starting with {}; a commit that arrived by git push is imported only when the push is accepted",
                commit
            )))
        }
    }
    Ok(())
}

/// Show who signed off on a changeset.
pub fn cmd_approvals(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let signers = crate::approval::approvers(&conn, id)?;
    if signers.is_empty() {
        println!("{} has no approvals", id);
        return Ok(());
    }
    println!("{} approved by:", id);
    for s in signers {
        println!("  {}", s);
    }
    Ok(())
}

/// Print the conflicts of a merge attempt as JSON.
pub fn cmd_branch_merge_json(source: &str, target: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = store::with_retry(|| repo::merge_branch(&conn, &root, source, target, author))?;
    emit(crate::json::Json::obj(vec![
        (
            "merged",
            crate::json::Json::Bool(out.merge_changeset.is_some()),
        ),
        (
            "changeset",
            out.merge_changeset
                .clone()
                .map(crate::json::Json::str)
                .unwrap_or(crate::json::Json::Null),
        ),
        (
            "applied",
            crate::json::Json::arr(
                out.applied
                    .iter()
                    .cloned()
                    .map(crate::json::Json::str)
                    .collect(),
            ),
        ),
        ("conflicts", crate::conflict::reports_to_json(&out.reports)),
    ]));
    // A conflicted merge is still a failure for a script, so keep the exit code.
    if out.merge_changeset.is_none() && !out.reports.is_empty() {
        return Err(bad("merge produced conflicts"));
    }
    Ok(())
}

/// Report the conflicts of a merge without performing it (dry run).
///
/// An agent asks this first, decides how to resolve, then writes its answer.
pub fn cmd_merge_preview(source: &str, target: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = repo::merge_preview(&conn, &root, source, target)?;
    emit(crate::conflict::reports_to_json(&out));
    Ok(())
}

pub fn cmd_branch_merge(source: &str, target: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = store::with_retry(|| repo::merge_branch(&conn, &root, source, target, author))?;

    if !out.conflicts.is_empty() {
        eprintln!(
            "merge stopped: {} conflicted file(s). Nothing was committed and the head did not move.",
            out.conflicts.len()
        );
        for c in &out.conflicts {
            eprintln!("  conflict: {} (their version saved as {}.theirs)", c, c);
        }
        eprintln!("\nResolve the files, then commit the result with 'changeset create'.");
        return Err(bad("merge produced conflicts"));
    }

    if out.applied.is_empty() {
        println!(
            "nothing to merge: {} has no changesets {} lacks",
            source, target
        );
        return Ok(());
    }
    ops::record(&conn, "merge", out.merge_changeset.as_deref(), Some(source))?;
    println!(
        "merged {} into {}: {} changeset(s)",
        source,
        target,
        out.applied.len()
    );
    for id in &out.applied {
        println!("  {}", id);
    }
    Ok(())
}

pub fn cmd_branch_info(name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (head, ts, from): (Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT head, created_at, from_branch FROM branches WHERE name = ?1",
            rusqlite::params![name],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| bad(format!("no such branch: {}{}", name, branch_names(&conn))))?;

    println!("branch:      {}", name);
    println!("created:     {}", fmt_ts(ts));
    println!("from:        {}", from.unwrap_or_else(|| "-".into()));
    println!(
        "head:        {}",
        head.clone().unwrap_or_else(|| "(empty)".into())
    );
    println!("changesets:  {}", reachable_from(&conn, name)?.len());
    if let (Some(h), Some(other)) = (&head, None::<String>) {
        let _ = (h, other);
    }
    Ok(())
}

/// Shorthand for the JSON command return type.
type YResultAlias = ZResult<()>;

// JSON output. Each function mirrors the text command's data exactly; the text
// version stays the default because a terminal is still the common case.

/// The version of every JSON document this program prints.
///
/// An agent reads this output and branches on its field names. Renaming a field without saying
/// so breaks a caller silently, which is the worst kind of break: it keeps running and does the
/// wrong thing. The version goes on every object so a caller can refuse one it does not know.
pub const SCHEMA_VERSION: &str = "gesta@1";

/// Set once anything has been printed as JSON, so a later failure does not append a second
/// object to a document that already went out.
pub static JSON_PRINTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Emit an object built outside this module, for a caller that needs the same schema stamp.
pub fn emit_object(fields: Vec<(&str, Json)>) {
    emit(Json::obj(fields));
}

fn emit(v: Json) {
    JSON_PRINTED.store(true, std::sync::atomic::Ordering::Relaxed);
    println!("{}", with_schema(v).render_pretty());
}

/// Wrap a list in an object that names the schema.
///
/// A bare array cannot carry a version, and an agent reading field names needs one. The bare
/// form stays the default because a pipe through `jq '.[0]'` is what people already write, so
/// the envelope is opt in rather than a silent change of shape.
pub fn envelope(key: &str, items: Vec<Json>) -> Json {
    let n = items.len() as i64;
    Json::obj(vec![
        ("schema", Json::str(SCHEMA_VERSION)),
        ("count", Json::int(n)),
        (key, Json::arr(items)),
    ])
}

/// Put `schema` at the top level of an object. Arrays and scalars carry nothing to version.
fn with_schema(v: Json) -> Json {
    match v {
        Json::Obj(mut m) => {
            // A document that already names its schema keeps it: a nested object printing its
            // own version must not be overwritten by the outer one.
            m.entry("schema".to_string())
                .or_insert_with(|| Json::str(SCHEMA_VERSION));
            Json::Obj(m)
        }
        other => other,
    }
}

pub fn cmd_status_json() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    let head = branch_head(&conn, &branch)?;
    let d = repo::diff_worktree(&conn, &root, &branch)?;

    let pending: Vec<Json> = {
        let mut stmt = conn.prepare(
            "SELECT id, message, author, created_at FROM changesets
             WHERE status='pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Json::obj(vec![
                ("id", Json::str(r.get::<_, String>(0)?)),
                ("message", Json::str(r.get::<_, String>(1)?)),
                ("author", Json::str(r.get::<_, String>(2)?)),
                ("created_at", Json::int(r.get::<_, i64>(3)?)),
            ]))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    emit(Json::obj(vec![
        ("branch", Json::str(branch)),
        ("head", head.map(Json::str).unwrap_or(Json::Null)),
        ("pending_changesets", Json::arr(pending)),
        (
            "added",
            Json::arr(d.added.into_iter().map(Json::str).collect()),
        ),
        (
            "modified",
            Json::arr(d.modified.into_iter().map(Json::str).collect()),
        ),
        (
            "deleted",
            Json::arr(d.deleted.into_iter().map(Json::str).collect()),
        ),
        (
            "skipped_binary",
            Json::arr(d.skipped_binary.into_iter().map(Json::str).collect()),
        ),
        ("unchanged", Json::int(d.unchanged as i64)),
    ]));
    Ok(())
}

pub fn cmd_log_json(limit: usize, envelope: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let mut stmt = conn.prepare(
        "SELECT id, message, author, created_at, status, parent FROM changesets
         ORDER BY created_at DESC, rowid DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
        Ok(Json::obj(vec![
            ("id", Json::str(r.get::<_, String>(0)?)),
            ("message", Json::str(r.get::<_, String>(1)?)),
            ("author", Json::str(r.get::<_, String>(2)?)),
            ("created_at", Json::int(r.get::<_, i64>(3)?)),
            ("status", Json::str(r.get::<_, String>(4)?)),
            (
                "parent",
                r.get::<_, Option<String>>(5)?
                    .map(Json::str)
                    .unwrap_or(Json::Null),
            ),
        ]))
    })?;
    let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if envelope {
        emit(self::envelope("changesets", items));
    } else {
        emit(Json::arr(items));
    }
    Ok(())
}

pub fn cmd_blame_json(file: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let rel = normalise(file);
    let mut stmt = conn.prepare(
        "SELECT b.start_line, b.end_line, b.changeset_id, c.author FROM blame_ranges b
         LEFT JOIN changesets c ON c.id = b.changeset_id WHERE b.path = ?1 ORDER BY b.start_line",
    )?;
    // The document keeps one entry per line, because a caller that asked for JSON asked for the
    // same answer the text form gives. Ranges are how the answer is stored, not how it is read.
    let ranges = stmt
        .query_map(rusqlite::params![rel], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut lines = Vec::new();
    for (start, end, cs, author) in ranges {
        for line in start..=end {
            lines.push(Json::obj(vec![
                ("line", Json::int(line)),
                ("changeset", Json::str(cs.clone())),
                ("author", Json::str(author.clone().unwrap_or_default())),
            ]));
        }
    }
    let count = lines.len();
    emit(Json::obj(vec![
        ("path", Json::str(rel)),
        ("line_count", Json::int(count as i64)),
        ("lines", Json::arr(lines)),
    ]));
    Ok(())
}

pub fn cmd_history_json(file: &str) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let rel = normalise(file);
    let chain = history::rename_history(&conn, &rel, 10)?;

    let mut versions = Vec::new();
    for name in &chain {
        let mut stmt = conn.prepare(
            "SELECT fv.path, fv.version, fv.hash, fv.created_at, cs.id, cs.message, cs.author
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |r| {
            Ok(Json::obj(vec![
                ("path", Json::str(r.get::<_, String>(0)?)),
                ("version", Json::int(r.get::<_, i64>(1)?)),
                ("hash", Json::str(r.get::<_, String>(2)?)),
                ("created_at", Json::int(r.get::<_, i64>(3)?)),
                ("changeset", Json::str(r.get::<_, String>(4)?)),
                ("message", Json::str(r.get::<_, String>(5)?)),
                ("author", Json::str(r.get::<_, String>(6)?)),
            ]))
        })?;
        for r in rows {
            versions.push(r?);
        }
    }

    emit(Json::obj(vec![
        ("path", Json::str(rel)),
        (
            "rename_chain",
            Json::arr(chain.into_iter().map(Json::str).collect()),
        ),
        ("versions", Json::arr(versions)),
    ]));
    Ok(())
}

pub fn cmd_diff_json(changeset: Option<&str>) -> YResultAlias {
    let (root, conn) = open_repo()?;
    match changeset {
        Some(id) => {
            let files = repo::changeset_diff(&conn, id)?;
            let renames = history::detect_renames(&conn, id, 0.5)?;
            emit(Json::obj(vec![
                ("changeset", Json::str(id)),
                (
                    "files",
                    Json::arr(
                        files
                            .into_iter()
                            .map(|(path, removed, added)| {
                                Json::obj(vec![
                                    ("path", Json::str(path)),
                                    (
                                        "removed",
                                        Json::arr(removed.into_iter().map(Json::str).collect()),
                                    ),
                                    (
                                        "added",
                                        Json::arr(added.into_iter().map(Json::str).collect()),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "renames",
                    Json::arr(
                        renames
                            .into_iter()
                            .map(|r| {
                                Json::obj(vec![
                                    ("from", Json::str(r.from)),
                                    ("to", Json::str(r.to)),
                                    ("similarity", Json::Float(r.similarity)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]));
        }
        None => {
            let branch = current_branch(&conn)?;
            let d = repo::diff_worktree(&conn, &root, &branch)?;
            emit(Json::obj(vec![
                ("branch", Json::str(branch)),
                (
                    "added",
                    Json::arr(d.added.into_iter().map(Json::str).collect()),
                ),
                (
                    "modified",
                    Json::arr(d.modified.into_iter().map(Json::str).collect()),
                ),
                (
                    "deleted",
                    Json::arr(d.deleted.into_iter().map(Json::str).collect()),
                ),
                (
                    "skipped_binary",
                    Json::arr(d.skipped_binary.into_iter().map(Json::str).collect()),
                ),
                ("unchanged", Json::int(d.unchanged as i64)),
            ]));
        }
    }
    Ok(())
}

pub fn cmd_tag_json() -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let mut stmt = conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok(Json::obj(vec![
            ("name", Json::str(r.get::<_, String>(0)?)),
            ("changeset", Json::str(r.get::<_, String>(1)?)),
            ("created_at", Json::int(r.get::<_, i64>(2)?)),
        ]))
    })?;
    emit(Json::arr(rows.collect::<rusqlite::Result<Vec<_>>>()?));
    Ok(())
}

pub fn cmd_ops_json(limit: usize) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let ops = ops::list(&conn, limit)?;
    emit(Json::arr(
        ops.into_iter()
            .map(|o| {
                Json::obj(vec![
                    ("id", Json::int(o.id)),
                    ("kind", Json::str(o.kind)),
                    ("target", o.target.map(Json::str).unwrap_or(Json::Null)),
                    ("detail", o.detail.map(Json::str).unwrap_or(Json::Null)),
                    ("created_at", Json::int(o.created_at)),
                ])
            })
            .collect(),
    ));
    Ok(())
}

pub fn cmd_branch_json(sub: crate::BranchCmd) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    match sub {
        crate::BranchCmd::List => {
            let cur = current_branch(&conn)?;
            let mut stmt =
                conn.prepare("SELECT name, head, created_at FROM branches ORDER BY name")?;
            let rows = stmt.query_map([], |r| {
                Ok(Json::obj(vec![
                    ("name", Json::str(r.get::<_, String>(0)?)),
                    (
                        "head",
                        r.get::<_, Option<String>>(1)?
                            .map(Json::str)
                            .unwrap_or(Json::Null),
                    ),
                    ("created_at", Json::int(r.get::<_, i64>(2)?)),
                ]))
            })?;
            let list = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            emit(Json::obj(vec![
                ("current", Json::str(cur)),
                ("branches", Json::arr(list)),
            ]));
            Ok(())
        }
        other => {
            // JSON mode is for reading state. A write still runs and prints its
            // normal text, because scripting a mutation was never the point.
            crate::cli::dispatch_branch_text(other)
        }
    }
}

pub fn cmd_changeset_json(sub: crate::ChangesetCmd) -> YResultAlias {
    match sub {
        crate::ChangesetCmd::List => {
            let (_root, conn) = open_repo()?;
            let mut stmt = conn.prepare(
                "SELECT id, message, author, created_at FROM changesets
                 WHERE status='pending' ORDER BY created_at, rowid",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Json::obj(vec![
                    ("id", Json::str(r.get::<_, String>(0)?)),
                    ("message", Json::str(r.get::<_, String>(1)?)),
                    ("author", Json::str(r.get::<_, String>(2)?)),
                    ("created_at", Json::int(r.get::<_, i64>(3)?)),
                ]))
            })?;
            emit(Json::arr(rows.collect::<rusqlite::Result<Vec<_>>>()?));
            Ok(())
        }
        crate::ChangesetCmd::Create {
            message,
            author,
            paths,
            session,
            intent,
        } => cmd_changeset_create_json(
            &message,
            &author,
            session.as_deref(),
            intent.as_deref(),
            &paths,
        ),
        crate::ChangesetCmd::Commit { id } => cmd_changeset_commit_json(&id),
        // A caller that asked for JSON and got text has a silent failure: it parses nothing and
        // branches on nothing, and the run looks successful. Refuse rather than print prose.
        _other => Err(Box::new(crate::error::Error::new(
            crate::error::Code::Usage,
            "this subcommand has no JSON form yet: run it without --json and read the text",
        ))),
    }
}

/// `changeset create --json`: the id and what it took, so a caller can act without parsing text.
fn cmd_changeset_create_json(
    message: &str,
    author: &str,
    session: Option<&str>,
    intent: Option<&str>,
    paths: &[String],
) -> YResultAlias {
    let (root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    let made =
        store::with_retry(|| repo::create_changeset_scoped(&conn, &root, message, author, paths))?;
    let Some(cs) = made else {
        // Nothing to snapshot is not an error, and a caller still needs a shape to read.
        emit(Json::obj(vec![
            ("changed", Json::Bool(false)),
            ("changeset", Json::Null),
            ("branch", Json::str(branch)),
        ]));
        return Ok(());
    };
    if session.is_some() || intent.is_some() {
        conn.execute(
            "UPDATE changesets SET session = ?1, intent = ?2 WHERE id = ?3",
            rusqlite::params![session, intent, cs.id],
        )?;
    }
    let entries = |v: &[(String, Option<String>, String)]| {
        Json::arr(
            v.iter()
                .map(|(rel, old, _)| {
                    Json::obj(vec![
                        ("path", Json::str(rel.clone())),
                        (
                            "change",
                            Json::str(if old.is_none() { "added" } else { "modified" }),
                        ),
                    ])
                })
                .collect(),
        )
    };
    emit(Json::obj(vec![
        ("changed", Json::Bool(true)),
        ("changeset", Json::str(cs.id.clone())),
        ("branch", Json::str(branch)),
        ("files", entries(&cs.files)),
        (
            "deleted",
            Json::arr(cs.deleted.iter().cloned().map(Json::str).collect()),
        ),
        (
            "skipped_binary",
            Json::arr(cs.skipped.iter().cloned().map(Json::str).collect()),
        ),
    ]));
    Ok(())
}

/// `changeset commit --json`: how many files moved, and the branch it landed on.
fn cmd_changeset_commit_json(id: &str) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let written = store::with_retry(|| repo::commit_one(&conn, id))?;
    emit(Json::obj(vec![
        ("id", Json::str(id)),
        ("files", Json::int(written as i64)),
        ("already_committed", Json::Bool(written == 0)),
    ]));
    Ok(())
}

// Text-mode dispatch for the subcommands JSON mode does not cover. Keeping it
// here means `main.rs` does not need to know which subcommands have a JSON form.
pub fn dispatch_branch_text(sub: crate::BranchCmd) -> ZResult<()> {
    match sub {
        crate::BranchCmd::Create { name, from } => cmd_branch_create(&name, &from),
        crate::BranchCmd::List => cmd_branch_list(),
        crate::BranchCmd::Switch { name } => cmd_branch_switch(&name),
        crate::BranchCmd::Delete { name } => cmd_branch_delete(&name),
        crate::BranchCmd::Merge {
            source,
            target,
            author,
        } => cmd_branch_merge(&source, &target, &author),
        crate::BranchCmd::Info { name } => cmd_branch_info(&name),
        crate::BranchCmd::Protect { name, unset } => cmd_branch_protect(&name, unset),
        crate::BranchCmd::Conflicts { source, target } => cmd_merge_preview(&source, &target),
    }
}

pub fn cmd_stash(sub: crate::StashCmd, json: bool) -> ZResult<()> {
    use crate::StashCmd;
    match sub {
        StashCmd::Save { message, author } => {
            let (root, conn) = open_repo()?;
            let (id, n) = crate::stash::save(&conn, &root, &message, &author)?;
            if n == 0 {
                println!("nothing to stash: the working tree matches the head");
                return Ok(());
            }
            if json {
                println!(
                    "{{\"id\":{},\"files\":{},\"message\":\"{}\"}}",
                    id,
                    n,
                    message.replace('"', "\\\"")
                );
            } else {
                println!("saved stash {} ({} file(s)): {}", id, n, message);
                println!("the working tree is back at the head");
            }
            Ok(())
        }
        StashCmd::List => {
            let (_root, conn) = open_repo()?;
            let all = crate::stash::list(&conn)?;
            if json {
                let mut parts = Vec::new();
                for s in &all {
                    parts.push(format!(
                        "{{\"id\":{},\"message\":\"{}\",\"files\":{},\"created_at\":{}}}",
                        s.id,
                        s.message.replace('"', "\\\""),
                        s.files.len(),
                        s.created_at
                    ));
                }
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if all.is_empty() {
                println!("no stashes");
                return Ok(());
            }
            for s in &all {
                println!(
                    "stash {}  {}  {} file(s)  {}",
                    s.id,
                    fmt_ts(s.created_at),
                    s.files.len(),
                    s.message
                );
            }
            Ok(())
        }
        StashCmd::Pop { id } => {
            let (root, conn) = open_repo()?;
            let (s, written, blocked) = crate::stash::pop(&conn, &root, id)?;
            if !blocked.is_empty() {
                // Refusing is the safe answer: writing over a path the working
                // tree has changed would destroy work that was never recorded.
                println!("stash {} not applied: these paths have local changes", s.id);
                for p in &blocked {
                    println!("  {}", p);
                }
                println!("commit or restore them first, then pop again");
                return Ok(());
            }
            println!(
                "applied stash {} ({} file(s)): {}",
                s.id, written, s.message
            );
            Ok(())
        }
        StashCmd::Drop { id } => {
            let (_root, conn) = open_repo()?;
            let target = match id {
                Some(i) => i,
                None => {
                    let all = crate::stash::list(&conn)?;
                    all.first()
                        .map(|s| s.id)
                        .ok_or_else(|| bad("no stash to drop".to_string()))?
                }
            };
            crate::stash::drop_stash(&conn, target)?;
            println!("dropped stash {}", target);
            Ok(())
        }
    }
}

pub fn cmd_hook(sub: crate::HookCmd, json: bool) -> ZResult<()> {
    use crate::HookCmd;
    match sub {
        HookCmd::List => {
            let (root, _conn) = open_repo()?;
            for (name, installed) in crate::hooks::list(&root) {
                println!(
                    "{}  {}",
                    if installed { "installed" } else { "absent   " },
                    name
                );
            }
            Ok(())
        }
        HookCmd::Install { name, file } => {
            let (root, _conn) = open_repo()?;
            let body = match file {
                Some(path) => std::fs::read_to_string(&path)
                    .map_err(|e| bad(format!("cannot read {}: {}", path, e)))?,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };
            if body.trim().is_empty() {
                return Err(bad("a hook needs a body: pass --file or write it on stdin"));
            }
            let path = crate::hooks::install(&root, &name, &body)?;
            println!("installed {} at {}", name, path.display());
            Ok(())
        }
        HookCmd::Status { json_status } => cmd_hook_status(json_status || json),
        HookCmd::Remove { name } => {
            let (root, _conn) = open_repo()?;
            if crate::hooks::remove(&root, &name)? {
                println!("removed {}", name);
            } else {
                println!("{} was not installed", name);
            }
            Ok(())
        }
    }
}

/// Report whether each hook still matches what was installed.
///
/// A hook runs with the user's rights. The interesting event is a hook that changed after it was
/// installed, because that is either a deliberate edit somebody should see or an edit nobody
/// asked for. Both deserve to be visible before the next commit runs it.
pub fn cmd_hook_status(json: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let states = crate::hooks::status(&root)?;
    if json {
        let mut items = Vec::new();
        for s in &states {
            items.push(crate::json::Json::obj(vec![
                ("name", crate::json::Json::str(s.name.clone())),
                ("installed", crate::json::Json::Bool(s.installed)),
                ("changed", crate::json::Json::Bool(s.changed())),
                (
                    "recorded",
                    match &s.recorded {
                        Some(h) => crate::json::Json::str(h.clone()),
                        None => crate::json::Json::Null,
                    },
                ),
                (
                    "current",
                    match &s.current {
                        Some(h) => crate::json::Json::str(h.clone()),
                        None => crate::json::Json::Null,
                    },
                ),
            ]));
        }
        emit_object(vec![
            ("hooks", crate::json::Json::Arr(items)),
            (
                "changed",
                crate::json::Json::int(states.iter().filter(|s| s.changed()).count() as i64),
            ),
        ]);
        return Ok(());
    }
    for s in &states {
        if !s.installed {
            println!("absent     {}", s.name);
        } else if s.changed() {
            println!("changed    {}", s.name);
        } else if s.recorded.is_some() {
            println!("unchanged  {}", s.name);
        } else {
            println!("unrecorded {}", s.name);
        }
    }
    let changed = states.iter().filter(|s| s.changed()).count();
    if changed > 0 {
        println!("{} hook(s) changed since installation", changed);
    }
    Ok(())
}

pub fn cmd_bisect(sub: crate::BisectCmd) -> ZResult<()> {
    use crate::BisectCmd;
    match sub {
        BisectCmd::Start {
            good,
            bad: bad_point,
        } => {
            let (root, conn) = open_repo()?;
            if crate::bisect::load(&conn)?.is_some() {
                return Err(bad(
                    "a bisect is already running: answer it or run `gesta bisect cancel`"
                        .to_string(),
                ));
            }
            let s = crate::bisect::start(&conn, &good, &bad_point)?;
            println!(
                "bisecting {} candidate(s) between {} and {}",
                s.candidates.len(),
                &good[..good.len().min(14)],
                &bad_point[..bad_point.len().min(14)]
            );
            // The first question comes right away, so the caller never has to
            // make a separate "what next" call before testing anything.
            match crate::bisect::step(&conn, &root, true)? {
                crate::bisect::Step::Test(id) => {
                    println!("testing {}", id);
                    println!("the working tree now holds that changeset: run your test, then");
                    println!("  gesta bisect good    if it passes");
                    println!("  gesta bisect bad     if it fails");
                }
                crate::bisect::Step::Found(id) => {
                    crate::bisect::finish(&conn, &root)?;
                    println!("first bad changeset: {}", id);
                }
            }
            Ok(())
        }
        BisectCmd::Good | BisectCmd::Bad => {
            let is_good = matches!(sub, BisectCmd::Good);
            let (root, conn) = open_repo()?;
            match crate::bisect::step(&conn, &root, is_good)? {
                crate::bisect::Step::Test(id) => {
                    println!("testing {}", id);
                    Ok(())
                }
                crate::bisect::Step::Found(id) => {
                    // Restoring the tree is part of finishing, not a separate
                    // thing to remember.
                    crate::bisect::finish(&conn, &root)?;
                    println!("first bad changeset: {}", id);
                    println!("the working tree is back at the branch head");
                    if let Ok(Some((msg, author))) = conn
                        .query_row(
                            "SELECT message, author FROM changesets WHERE id = ?1",
                            rusqlite::params![id],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                        )
                        .map(Some)
                    {
                        println!("  {} by {}", msg, author);
                    }
                    Ok(())
                }
            }
        }
        BisectCmd::Status => {
            let (_root, conn) = open_repo()?;
            match crate::bisect::load(&conn)? {
                None => {
                    println!("no bisect running");
                    Ok(())
                }
                Some(s) => {
                    println!("good:       {}", s.good);
                    println!("bad:        {}", s.bad);
                    println!("remaining:  {} candidate(s)", s.candidates.len());
                    println!("questions:  {}", s.steps);
                    if let Some(cur) = s.current {
                        println!("testing:    {}", cur);
                    }
                    Ok(())
                }
            }
        }
        BisectCmd::Cancel => {
            let (root, conn) = open_repo()?;
            if crate::bisect::load(&conn)?.is_none() {
                println!("no bisect running");
                return Ok(());
            }
            let n = crate::bisect::finish(&conn, &root)?;
            println!("bisect cancelled, {} file(s) restored to the head", n);
            Ok(())
        }
    }
}

pub fn cmd_cherry_pick(source: &str, onto: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let target = match onto {
        Some(b) => b.to_string(),
        None => current_branch(&conn)?,
    };
    let out = repo::cherry_pick(&conn, &root, source, &target, author)?;

    if !out.conflicts.is_empty() {
        println!(
            "cherry-pick stopped: {} file(s) conflict",
            out.conflicts.len()
        );
        for p in &out.conflicts {
            println!("  {}", p);
        }
        println!("the head is unchanged; their side was written beside each file as .theirs");
        return Err(bad("cherry-pick refused the conflicted paths".to_string()));
    }
    match out.merge_changeset {
        Some(id) => {
            println!("picked {} onto {} as {}", source, target, id);
            Ok(())
        }
        None => {
            println!(
                "nothing to pick: the change is already present on {}",
                target
            );
            Ok(())
        }
    }
}

pub fn cmd_rebase(
    onto: Option<&str>,
    branch: Option<&str>,
    author: &str,
    do_continue: bool,
    do_abort: bool,
) -> ZResult<()> {
    let (root, conn) = open_repo()?;

    if do_abort {
        let name = crate::rebase::abort(&conn, &root)?;
        println!("rebase aborted, {} is back where it started", name);
        return Ok(());
    }

    if do_continue {
        report_rebase(crate::rebase::continue_after_resolution(
            &conn, &root, author,
        )?);
        return Ok(());
    }

    let onto = onto.ok_or_else(|| {
        bad("rebase needs a point to move onto, or --continue / --abort".to_string())
    })?;
    let target = match branch {
        Some(b) => b.to_string(),
        None => current_branch(&conn)?,
    };
    let progress = crate::rebase::start(&conn, &root, &target, onto, author)?;
    report_rebase(progress);
    Ok(())
}

fn report_rebase(p: crate::rebase::Progress) {
    use crate::rebase::Progress;
    match p {
        Progress::Nothing => {
            println!("nothing to rebase: the base already contains this branch");
        }
        Progress::Done(ids) if ids.is_empty() => {
            println!("rebase finished: nothing needed recording");
        }
        Progress::Done(ids) => {
            println!("rebased {} changeset(s) onto the new base", ids.len());
        }
        Progress::Stopped { conflicts } => {
            println!("rebase stopped: {} file(s) conflict", conflicts.len());
            for c in &conflicts {
                println!("  {}", c);
            }
            println!();
            println!("resolve them, then:");
            println!("  gesta rebase --continue   keeps the resolution as a changeset");
            println!("  gesta rebase --abort      puts the branch back");
        }
    }
}

pub fn cmd_sub(sub: crate::SubCmd, json: bool) -> ZResult<()> {
    use crate::SubCmd;
    match sub {
        SubCmd::Add { path, remote } => {
            let (_root, conn) = open_repo()?;
            let s = crate::subrepo::add(&conn, &path, &remote)?;
            println!("added subrepository {} -> {}", s.path, s.remote);
            println!(
                "next: place a checkout there, then `gesta sub pin {}`",
                s.path
            );
            Ok(())
        }
        SubCmd::List => {
            let (root, conn) = open_repo()?;
            let all = crate::subrepo::list(&conn)?;
            if json {
                let parts: Vec<String> = all
                    .iter()
                    .map(|s| {
                        format!(
                            "{{\"path\":\"{}\",\"remote\":\"{}\",\"pinned\":\"{}\",\"present\":{}}}",
                            s.path,
                            s.remote,
                            s.pinned,
                            crate::subrepo::is_present(&root, s)
                        )
                    })
                    .collect();
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if all.is_empty() {
                println!("no subrepositories");
                return Ok(());
            }
            for s in &all {
                // A missing checkout is stated, not hidden: a caller that acts on
                // the pin needs to know the directory is not there.
                let state = if crate::subrepo::is_present(&root, s) {
                    "present"
                } else {
                    "missing"
                };
                let pin = if s.pinned.is_empty() {
                    "unpinned".to_string()
                } else {
                    s.pinned.clone()
                };
                println!("{}  {}  {}  {}", s.path, pin, state, s.remote);
            }
            Ok(())
        }
        SubCmd::Pin { path } => {
            let (root, conn) = open_repo()?;
            let s = crate::subrepo::get(&conn, &path)?
                .ok_or_else(|| bad(format!("{} is not a subrepository", path)))?;
            let head = crate::subrepo::child_head(&root, &s).ok_or_else(|| {
                bad(format!(
                    "{} has no repository checkout, so there is nothing to pin",
                    s.path
                ))
            })?;
            let s = crate::subrepo::pin(&conn, &path, &head)?;
            println!("pinned {} at {}", s.path, s.pinned);
            Ok(())
        }
        SubCmd::Remove { path } => {
            let (_root, conn) = open_repo()?;
            if crate::subrepo::remove(&conn, &path)? {
                println!("removed subrepository {}", path);
            } else {
                println!("{} was not a subrepository", path);
            }
            Ok(())
        }
    }
}

/// Read a line from the terminal without echoing it.
///
/// The value is typed, not passed as an argument: an argument is visible in the
/// process list and lands in shell history, and a secret in either is a secret that
/// has to be rotated.
fn read_hidden(prompt: &str) -> ZResult<String> {
    use std::io::{BufRead, Write};

    print!("{}", prompt);
    std::io::stdout().flush()?;

    #[cfg(unix)]
    {
        // stty is present wherever a terminal is, and turning echo off for one read
        // is the one thing this needs from it.
        let _ = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        println!();
        #[allow(clippy::needless_return)] // the other cfg branch returns below
        return Ok(line.trim_end().to_string());
    }

    #[cfg(not(unix))]
    {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim_end().to_string())
    }
}

/// Handle `gesta submodule`.
pub fn cmd_submodule(sub: crate::SubmoduleCmd, json: bool) -> ZResult<()> {
    use crate::SubmoduleCmd;
    match sub {
        SubmoduleCmd::Add { path, url, branch } => {
            let (root, _conn) = open_repo()?;
            let mut subs = crate::submodule::read_gitmodules(&root)?;
            if subs.contains_key(&path) {
                return Err(bad(format!("submodule {} already exists", path)));
            }
            subs.insert(
                path.clone(),
                crate::submodule::Submodule {
                    path: path.clone(),
                    url: url.clone(),
                    branch,
                    commit: None,
                },
            );
            crate::submodule::write_gitmodules(&root, &subs)?;
            println!("added submodule {} -> {}", path, url.clone());
            println!("run `gesta submodule init` to clone it");
            Ok(())
        }
        SubmoduleCmd::Init => {
            let (root, _conn) = open_repo()?;
            let cloned = crate::submodule::submodule_init(&root)?;
            if cloned.is_empty() {
                println!("all submodules already initialized");
            } else {
                for c in cloned {
                    println!("cloned {}", c);
                }
            }
            Ok(())
        }
        SubmoduleCmd::Update => {
            let (root, _conn) = open_repo()?;
            let updated = crate::submodule::submodule_update(&root)?;
            if updated.is_empty() {
                println!("all submodules up to date");
            } else {
                for u in updated {
                    println!("updated {}", u);
                }
            }
            Ok(())
        }
        SubmoduleCmd::Status => {
            let (root, _conn) = open_repo()?;
            let statuses = crate::submodule::submodule_status(&root)?;
            if json {
                let parts: Vec<String> = statuses
                    .iter()
                    .map(|s| format!(
                        "{{\"path\":\"{}\",\"commit\":\"{}\",\"pinned\":{},                        \"dirty\":{},                        \"missing\":{}}}",
                        s.path,
                        s.commit,
                        s.pinned.as_ref().map(|p| format!("\"{}\"", p)).unwrap_or("null".to_string()),
                        s.dirty,
                        s.missing
                    ))
                    .collect();
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if statuses.is_empty() {
                println!("no submodules");
                return Ok(());
            }
            for s in &statuses {
                let state = if s.missing {
                    "missing"
                } else if s.dirty {
                    "dirty"
                } else if !crate::submodule::matches_pin(s.pinned.as_deref(), &s.commit) {
                    "behind"
                } else {
                    "clean"
                };
                let pin = s.pinned.as_deref().unwrap_or("unpinned");
                println!("{}  {}  {}  {}", s.path, s.commit, pin, state);
            }
            Ok(())
        }
        SubmoduleCmd::Sync => {
            let (root, _conn) = open_repo()?;
            let subs = crate::submodule::read_gitmodules(&root)?;
            for sub in subs.values() {
                println!("{}  {}", sub.path, sub.url);
            }
            Ok(())
        }
    }
}

/// Handle `gesta secret`.
pub fn cmd_secret(sub: crate::SecretCmd) -> ZResult<()> {
    use crate::SecretCmd;

    match sub {
        SecretCmd::Init => {
            let path = match std::env::var("GESTA_SECRETS_FILE") {
                Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
                _ => crate::secrets::default_path()?,
            };
            println!("store: {}", path.display());
            let first = read_hidden("passphrase: ")?;
            if first.len() < 8 {
                return Err(bad(
                    "a passphrase shorter than 8 characters is not worth the file it protects"
                        .to_string(),
                ));
            }
            let again = read_hidden("repeat: ")?;
            if first != again {
                return Err(bad("the two passphrases differ".to_string()));
            }
            let store = crate::secrets::SecretStore {
                path,
                passphrase: first,
            };
            let existing = store.load()?;
            // Saving an empty map is what creates the file, and keeps what is there.
            store.save(&existing)?;
            println!(
                "created {} ({} entries kept)",
                store.path.display(),
                existing.len()
            );
            Ok(())
        }
        SecretCmd::Set { name } => {
            let store = crate::secrets::SecretStore::from_env()?;
            let value = read_hidden(&format!("value for {}: ", name))?;
            if value.is_empty() {
                return Err(bad("an empty value is not stored".to_string()));
            }
            store.put(&name, &value)?;
            println!("stored {} in {}", name, store.path.display());
            Ok(())
        }
        SecretCmd::List => {
            let store = crate::secrets::SecretStore::from_env()?;
            let entries = store.load()?;
            if entries.is_empty() {
                println!("no entries in {}", store.path.display());
            } else {
                println!("{} ({} entries)", store.path.display(), entries.len());
                for name in entries.keys() {
                    println!("  {}", name);
                }
            }
            Ok(())
        }
        SecretCmd::Remove { name } => {
            let store = crate::secrets::SecretStore::from_env()?;
            if store.remove(&name)? {
                println!("removed {}", name);
            } else {
                println!("no entry named {}", name);
            }
            Ok(())
        }
        SecretCmd::Scan { history, json_scan } => {
            crate::cli_remote::cmd_secret_scan(history, json_scan)
        }
    }
}

/// `gesta diff --from A --to B`: compare two revisions.
/// What "before" means when a caller names only what "after" is.
///
/// `diff --to main` reads as "what is in main that differs", and the answer a caller wants is what
/// changed since its parent. Treating the missing side as the empty tree answers a different
/// question, and the caller discovers the difference by counting files. A first changeset has no
/// parent, so the empty tree is the right answer there and is returned.
fn resolve_from(
    conn: &rusqlite::Connection,
    from: Option<&str>,
    to: Option<&str>,
) -> ZResult<Option<String>> {
    // An explicit empty string means the empty tree, which is how a caller asks for everything in
    // a revision. Without it the earlier behaviour had no way to be requested at all, and removing
    // a capability to fix a default is not a fix.
    if let Some(f) = from {
        return Ok(if f.is_empty() {
            None
        } else {
            Some(f.to_string())
        });
    }
    let to = match to {
        Some(t) => t,
        // Neither side named: comparing nothing with nothing, which has no parent to resolve.
        None => return Ok(None),
    };
    match crate::repo::parent_of(conn, to)? {
        Some(parent) => Ok(Some(parent)),
        // A first changeset has no parent. The empty tree is then the honest answer, and the
        // function returns None rather than inventing a revision that does not exist.
        None => Ok(None),
    }
}

pub fn cmd_diff_range(from: Option<&str>, to: Option<&str>, name_only: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let from = resolve_from(&conn, from, to)?;
    let diffs = crate::repo::diff_revs(&conn, from.as_deref(), to)?;
    if diffs.is_empty() {
        println!(
            "no difference between {} and {}",
            from.as_deref().unwrap_or("an empty tree"),
            to.unwrap_or("an empty tree")
        );
        return Ok(());
    }
    for (path, removed, added) in diffs {
        if name_only {
            println!("{}", path);
            continue;
        }
        println!("\n--- {}", path);
        for l in &removed {
            println!("-{}", l);
        }
        for l in &added {
            println!("+{}", l);
        }
    }
    Ok(())
}

/// The same as `cmd_diff_range` with `--json`.
pub fn cmd_diff_range_json(from: Option<&str>, to: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let from = resolve_from(&conn, from, to)?;
    let diffs = crate::repo::diff_revs(&conn, from.as_deref(), to)?;
    use crate::json::Json;
    let arr = Json::arr(
        diffs
            .into_iter()
            .map(|(path, removed, added)| {
                Json::obj(vec![
                    ("path", Json::str(path)),
                    (
                        "removed",
                        Json::arr(removed.into_iter().map(Json::str).collect()),
                    ),
                    (
                        "added",
                        Json::arr(added.into_iter().map(Json::str).collect()),
                    ),
                ])
            })
            .collect(),
    );
    println!("{}", arr.render());
    Ok(())
}

/// `gesta review`: a stack of changesets, one step at a time, each with a risk read.
///
/// This is the answer to reviewing what an agent produced. A net diff between two
/// points hides the order things happened in, and the order is most of the reasoning.
/// The machine-readable review, which is what an agent should use.
///
/// A reviewer that parses the text has to reproduce the layout, and the layout exists for a person.
/// This carries the same answer as data: each step, its risk band and score, the reasons behind the
/// score, the files it touched, and the stale references found outside them.
pub fn cmd_review_json(
    from: Option<&str>,
    to: Option<&str>,
    min_risk: Option<&str>,
) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    let target = to.unwrap_or(&branch);

    let floor = match min_risk {
        Some("low") => 0,
        Some("medium") => 4,
        Some("high") => 9,
        Some(other) => {
            return Err(bad(format!(
                "unknown risk band: {} (use low, medium or high)",
                other
            )))
        }
        None => 0,
    };

    let steps = crate::repo::review_stack(&conn, from, target)?;
    // The same tree the text form reads, so the two cannot disagree about what a change affects.
    let tree = match crate::store::branch_head(&conn, target)? {
        Some(head) => crate::store::tree_at(&conn, &head)?,
        None => Vec::new(),
    };

    let mut items = Vec::new();
    let mut hidden = 0i64;
    for step in &steps {
        if step.risk.score < floor {
            hidden += 1;
            continue;
        }
        let files = step
            .files
            .iter()
            .map(|(path, removed, added)| {
                crate::json::Json::obj(vec![
                    ("path", crate::json::Json::str(path.clone())),
                    (
                        "removed",
                        crate::json::Json::arr(
                            removed
                                .iter()
                                .map(|l| crate::json::Json::str(l.clone()))
                                .collect(),
                        ),
                    ),
                    (
                        "added",
                        crate::json::Json::arr(
                            added
                                .iter()
                                .map(|l| crate::json::Json::str(l.clone()))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();

        let stale = crate::impact::stale_references(&conn, &step.files, &tree)
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                crate::json::Json::obj(vec![
                    ("name", crate::json::Json::str(s.name)),
                    ("path", crate::json::Json::str(s.path)),
                    ("line", crate::json::Json::int(s.line as i64)),
                    ("text", crate::json::Json::str(s.text)),
                    ("removed_from", crate::json::Json::str(s.removed_from)),
                ])
            })
            .collect();

        items.push(crate::json::Json::obj(vec![
            ("changeset", crate::json::Json::str(step.id.clone())),
            ("message", crate::json::Json::str(step.message.clone())),
            ("author", crate::json::Json::str(step.author.clone())),
            ("risk", crate::json::Json::str(step.risk.band())),
            ("score", crate::json::Json::int(step.risk.score)),
            (
                "reasons",
                crate::json::Json::arr(
                    step.risk
                        .reasons
                        .iter()
                        .map(|r| crate::json::Json::str(r.clone()))
                        .collect(),
                ),
            ),
            ("files", crate::json::Json::arr(files)),
            ("stale", crate::json::Json::arr(stale)),
        ]));
    }

    emit_object(vec![
        ("branch", crate::json::Json::str(target.to_string())),
        (
            "from",
            match from {
                Some(f) => crate::json::Json::str(f.to_string()),
                None => crate::json::Json::Null,
            },
        ),
        ("shown", crate::json::Json::int(items.len() as i64)),
        ("below_threshold", crate::json::Json::int(hidden)),
        ("steps", crate::json::Json::arr(items)),
    ]);
    Ok(())
}

pub fn cmd_review(from: Option<&str>, to: Option<&str>, min_risk: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let branch = current_branch(&conn)?;
    let target = to.unwrap_or(&branch);

    let steps = crate::repo::review_stack(&conn, from, target)?;
    if steps.is_empty() {
        println!(
            "nothing between {} and {}",
            from.unwrap_or("the start"),
            target
        );
        return Ok(());
    }

    let floor = match min_risk {
        Some("low") => 0,
        Some("medium") => 4,
        Some("high") => 9,
        Some(other) => {
            return Err(bad(format!(
                "unknown risk band: {} (use low, medium or high)",
                other
            )))
        }
        None => 0,
    };

    let mut shown = 0usize;
    let mut hidden = 0usize;
    for (n, step) in steps.iter().enumerate() {
        if step.risk.score < floor {
            hidden += 1;
            continue;
        }
        shown += 1;
        println!(
            "\n[{}] {}  {}  ({})",
            n + 1,
            &step.id[..step.id.len().min(14)],
            step.message,
            step.author
        );
        println!(
            "    risk {} ({})  {} file(s), {}",
            step.risk.band(),
            step.risk.score,
            step.files.len(),
            step.risk.reasons.join("; ")
        );
        for (path, removed, added) in &step.files {
            println!("    --- {}", path);
            for l in removed {
                println!("    -{}", l);
            }
            for l in added {
                println!("    +{}", l);
            }
        }

        // What the change breaks outside itself. A diff shows what was edited and says nothing
        // about what depended on it, and the failures that hurt are the ones where the change
        // looks complete and a file nobody touched stops working.
        if let Some(head) = crate::store::branch_head(&conn, target)? {
            let tree = crate::store::tree_at(&conn, &head)?;
            match crate::impact::stale_references(&conn, &step.files, &tree) {
                Ok(stale) if !stale.is_empty() => {
                    println!(
                        "    {} stale reference(s) in files this step did not touch:",
                        stale.len()
                    );
                    for s in stale.iter().take(5) {
                        println!(
                            "      {} line {}: `{}` was removed from {} and is still read here",
                            s.path, s.line, s.name, s.removed_from
                        );
                        println!("        {}", s.text);
                    }
                    if stale.len() > 5 {
                        println!(
                            "      ... {} more (gesta review --json lists every one)",
                            stale.len() - 5
                        );
                    }
                }
                Ok(_) => {
                    // Nothing found is not the same claim as nothing to find. Counting where a
                    // name could be built or forwarded says which of the two this is, and a
                    // reader who knows the gap decides differently about the clean result.
                    let blind = crate::impact::blind_spots(&conn, &tree);
                    if blind.any() {
                        println!(
                            "    no stale reference found, but {} file(s) could hide one: {} place(s) \
                             build a name at run time, {} forward it through another name",
                            blind.files_read, blind.computed_keys, blind.indirections
                        );
                    }
                }
                // A review that fails because a file could not be read would stop a reader
                // from seeing the findings it did produce. Say so and carry on.
                Err(e) => println!("    impact check skipped: {}", e),
            }
        }
    }
    println!(
        "\n{} step(s) shown, {} below the {} threshold",
        shown,
        hidden,
        min_risk.unwrap_or("low")
    );
    Ok(())
}

/// `gesta land`: merge one branch into another, but only if the target has not moved.
///
/// Without the check, two people landing at the same time both read the same head, both
/// merge against it, and the second overwrites the first with no error anywhere. A
/// compare-and-swap turns that into a refusal, which is the whole point of a queue.
pub fn cmd_land(source: &str, onto: &str, expect: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let before = store::branch_head(&conn, onto)?;
    if let Some(e) = expect {
        if before.as_deref() != Some(e) {
            return Err(bad(format!(
                "{} moved: expected {}, found {}. Re-read the head and prepare again.",
                onto,
                e,
                before.as_deref().unwrap_or("(none)")
            )));
        }
    }

    let out = crate::repo::land_branch(&conn, &root, source, onto, before.as_deref(), author)?;
    if !out.conflicts.is_empty() {
        eprintln!(
            "{} change(s) conflict when landing {} onto {}:",
            out.conflicts.len(),
            source,
            onto
        );
        for c in &out.conflicts {
            eprintln!("  {} (their version saved as {}.theirs)", c, c);
        }
        return Err(bad(
            "landing needs a manual merge; the target was not moved".to_string(),
        ));
    }
    if out.applied.is_empty() {
        println!("{} has nothing that {} does not already have", source, onto);
        return Ok(());
    }
    let after = store::branch_head(&conn, onto)?;

    // Landing moved the target head, so the working tree has to follow when the target is
    // the branch in front of the user. Without this the file the merge added exists in
    // history and is reported as deleted on disk, which looks like data loss.
    let current = current_branch(&conn)?;
    let refreshed = if current == onto {
        Some(repo::materialise(&conn, &root, onto)?)
    } else {
        None
    };

    ops::record(&conn, "land", after.as_deref(), Some(source))?;
    println!(
        "landed {} onto {} ({} change(s), head now {}{})",
        source,
        onto,
        out.applied.len(),
        after.as_deref().unwrap_or("(none)"),
        match refreshed {
            Some(n) => format!(", {} file(s) written", n),
            None => String::new(),
        }
    );
    Ok(())
}

#[cfg(test)]
mod pending_tests {
    use super::*;

    /// A pending changeset is a snapshot of an earlier moment. If the file moved on before it
    /// was committed, committing it stores content that does not describe the disk, and
    /// nothing else would say so.
    #[test]
    fn a_pending_changeset_that_no_longer_matches_the_disk_is_reported() {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
        crate::repo::tests::commit_files(&conn, "p1", "base", &[("f.txt", "v1\n")]);
        crate::store::set_branch_head(&conn, "main", "p1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();

        // Snapshot a change without committing it.
        std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
        let cs = crate::repo::create_changeset(&conn, &dir, "snapshot", "a")
            .unwrap()
            .expect("there is a change to snapshot");

        // Matching the disk, so nothing is stale yet.
        let clean = pending_out_of_date(&conn, &dir, 0).unwrap();
        assert!(
            clean.is_empty(),
            "a fresh snapshot must not be reported: {:?}",
            clean
        );

        // The file moves on before the commit.
        std::fs::write(dir.join("f.txt"), "v3\n").unwrap();
        let stale = pending_out_of_date(&conn, &dir, 0).unwrap();
        assert_eq!(stale.len(), 1, "the stale snapshot was not found");
        assert_eq!(stale[0].0, cs.id);
        assert_eq!(stale[0].1, vec!["f.txt".to_string()]);

        // Deleting the file is also a mismatch.
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        let gone = pending_out_of_date(&conn, &dir, 0).unwrap();
        assert_eq!(
            gone.len(),
            1,
            "a file removed after the snapshot was not reported"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
