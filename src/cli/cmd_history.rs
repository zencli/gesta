//! The history commands.

use super::*;

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

    let content = match walk::read_text(&root.join(&rel)) {
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
