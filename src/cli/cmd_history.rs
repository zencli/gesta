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
    //
    // "Before" is ordered by (created_at, rowid), not by created_at alone. Two commits in the same
    // second carry the same timestamp, so `created_at <=` let a changeset made *after* the ignored
    // one count as earlier, and its rowid tiebreak picked the later of the two. The ordering is
    // strict: a changeset is earlier only if its stamp is smaller, or equal with a smaller rowid.
    //
    // The walk continues while the answer is itself ignored. Skipping one revision when the caller
    // named three would report the second one, which is still a revision they asked not to see.
    //
    // There is deliberately no fallback to `current`. `current` is the row this branch was reached
    // for, so it is in the ignore list by definition; returning it answers with the changeset the
    // caller asked to skip. A line whose every writer is ignored has no earlier author, and `None`
    // says that.
    let (stamp, rid): (i64, i64) = conn.query_row(
        "SELECT created_at, rowid FROM changesets WHERE id = ?1",
        rusqlite::params![cs],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut at_stamp = stamp;
    let mut at_rowid = rid;
    let mut skip: Vec<String> = ignored.to_vec();
    loop {
        let earlier: Option<(String, String, Option<String>)> = conn
            .query_row(
                "SELECT cf.changeset_id, c.author, c.intent
                 FROM changeset_files cf JOIN changesets c ON c.id = cf.changeset_id
                 WHERE cf.path = ?1
                   AND (c.created_at < ?2 OR (c.created_at = ?2 AND c.rowid < ?3))
                 ORDER BY c.created_at DESC, c.rowid DESC LIMIT 1",
                rusqlite::params![path, at_stamp, at_rowid],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((found_cs, author, intent)) = earlier else {
            return Ok(None);
        };
        if !skip.iter().any(|i| i == &found_cs) {
            return Ok(Some((found_cs, author, intent)));
        }
        // Step past it: ask for the newest changeset strictly before this one.
        let (s2, r2): (i64, i64) = conn.query_row(
            "SELECT created_at, rowid FROM changesets WHERE id = ?1",
            rusqlite::params![found_cs],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        skip.push(found_cs);
        at_stamp = s2;
        at_rowid = r2;
    }
}

pub fn cmd_blame(file: &str, ignore: &[String]) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let rel = normalise(file);

    // Following a revision through a rewrite is what keeps blame useful. A tree-wide reformat or a
    // rename of every symbol makes every line point at one changeset, which is true and useless:
    // the reader wants the change that last touched the line in substance. Git settled on a file
    // at the repository root listing those revisions, read without being asked, and the same
    // convention here means a project does not have to teach every tool about its own list.
    let ignore = resolve_ignore_list(&root, ignore);
    let content = read_blame_content(&root, &conn, &rel)?;

    // A blame row answers who wrote a line. With provenance it can also answer which
    // session wrote it and why, which is the question that actually comes up when an
    // agent produced most of the file.
    let rows = read_blame_rows(&conn, &rel)?;

    if rows.is_empty() {
        println!("no blame data for {} (never committed)", rel);
        return Ok(());
    }
    let map: BlameMap = rows
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
        "SELECT id, message, author, created_at, status, parent, session, intent FROM changesets
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
            (
                "session",
                r.get::<_, Option<String>>(6)?
                    .map(Json::str)
                    .unwrap_or(Json::Null),
            ),
            (
                "intent",
                r.get::<_, Option<String>>(7)?
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

pub fn cmd_blame_json(file: &str, ignore: &[String]) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let rel = normalise(file);
    // Resolved the same way the text path resolves it, because the ignore list changes which
    // changeset a line is attributed to. Reading one path and letting the other skip the list is
    // how JSON and text blame end up naming different changesets for the same line.
    let ignore = resolve_ignore_list(&root, ignore);
    let content = read_blame_content(&root, &conn, &rel)?;
    let map = read_blame_map(&conn, &rel)?;

    let mut lines = Vec::new();
    for (i, _line) in content.lines().enumerate() {
        let n = (i + 1) as i64;
        let looked = match map.get(&n) {
            Some(found) if ignore.iter().any(|i| i == &found.0) => {
                blame_through(&conn, &rel, n, &ignore)?
            }
            Some(found) => Some(found.clone()),
            None => blame_through(&conn, &rel, n, &ignore)?,
        };
        let (cs, author) = match looked {
            Some((cs, author, _intent)) => (Json::str(cs), Json::str(author)),
            // A line the ignore list walked past every writer of has no changeset to name.
            // Reporting `null` says that; an empty string reads as an author with no name.
            None => (Json::Null, Json::Null),
        };
        lines.push(Json::obj(vec![
            ("line", Json::int(n)),
            ("changeset", cs),
            ("author", author),
        ]));
    }
    let count = lines.len();
    emit(Json::obj(vec![
        ("path", Json::str(rel)),
        ("line_count", Json::int(count as i64)),
        (
            "ignored",
            Json::arr(ignore.into_iter().map(Json::str).collect()),
        ),
        ("lines", Json::arr(lines)),
    ]));
    Ok(())
}

/// Turn the caller's `--ignore-rev` values into a list of changesets.
///
/// A value may name a revision directly or a file holding revisions, and with none given the
/// repository's own `.gesta-blame-ignore-revs` is read. Shared by the text and JSON paths so the
/// two cannot disagree about which changesets were skipped.
fn resolve_ignore_list(root: &Path, ignore: &[String]) -> Vec<String> {
    if ignore.is_empty() {
        return read_ignore_revs(root);
    }
    ignore
        .iter()
        .flat_map(|v| match std::fs::read_to_string(root.join(v)) {
            Ok(text) => parse_rev_list(&text),
            Err(_) => vec![v.clone()],
        })
        .collect()
}

/// The file's lines, from the working tree or, failing that, the last committed body.
fn read_blame_content(root: &Path, conn: &Connection, rel: &str) -> ZResult<String> {
    match walk::read_text(&root.join(rel)) {
        Some(c) => Ok(c),
        None => {
            let branch = current_branch(conn)?;
            let h = store::latest_hash(conn, rel, &branch)?
                .ok_or_else(|| bad(format!("file not found: {}", rel)))?;
            get_object(conn, &h)
        }
    }
}

type BlameRow = (i64, String, String, Option<String>);
type BlameMap = std::collections::HashMap<i64, (String, String, Option<String>)>;

fn read_blame_map(conn: &Connection, rel: &str) -> ZResult<BlameMap> {
    let rows = read_blame_rows(conn, rel)?;
    Ok(rows
        .into_iter()
        .map(|(n, c, a, i)| (n, (c, a, i)))
        .collect())
}

/// One entry per line, expanded from the stored ranges.
fn read_blame_rows(conn: &Connection, rel: &str) -> ZResult<Vec<BlameRow>> {
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
    Ok(out)
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
                ("file_hash", Json::str(r.get::<_, String>(2)?)),
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

#[cfg(test)]
mod ignore_rev_tests {
    use super::*;

    /// A connection with two changesets a second apart (or in the same second) and a one-line range
    /// blamed on the first.
    ///
    /// Written as SQL rather than through `changeset commit` because the point is the timestamps:
    /// the bug appears when the ignored revision is older by at least a second, and the product path
    /// would need a `sleep` to arrange that. This is the "fixture needs a state the product cannot
    /// create cheaply" case, not a fixture that skips a guard.
    fn two_changesets(gap_seconds: i64) -> Connection {
        let conn = crate::store::tests::mem_db();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, branch)
             VALUES('csolder', 'first', 'agent', 1000, 'committed', 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
             VALUES('csnewer', 'second', 'agent', ?1, 'committed', 'csolder', 'main')",
            [1000 + gap_seconds],
        )
        .unwrap();
        for cs in ["csolder", "csnewer"] {
            conn.execute(
                "INSERT INTO changeset_files(changeset_id, path) VALUES(?1, 'a.txt')",
                [cs],
            )
            .unwrap();
        }
        // Line 1 was written in `csolder` and never touched again; line 2 changed in `csnewer`.
        conn.execute(
            "INSERT INTO blame_ranges(path, start_line, end_line, changeset_id)
             VALUES('a.txt', 1, 1, 'csolder')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blame_ranges(path, start_line, end_line, changeset_id)
             VALUES('a.txt', 2, 2, 'csnewer')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn skipping_the_only_writer_of_a_line_does_not_blame_it_on_itself() {
        // It used to. The earlier-changeset query filtered `created_at <=` the ignored one and then
        // `earlier.or(current)` fell back to the ignored row itself, so a line whose only writer was
        // skipped came back attributed to the changeset the caller asked to skip. Whether it did
        // depended on whether the two changesets landed in the same second, which is why the bench
        // failed roughly one run in twenty and named a different pair each time.
        for gap in [0, 1, 5] {
            let conn = two_changesets(gap);
            let found = blame_through(&conn, "a.txt", 1, &["csolder".to_string()]).unwrap();
            match found {
                None => {} // nothing earlier wrote line 1, which is the honest answer
                Some((cs, _, _)) => assert_ne!(
                    cs, "csolder",
                    "with a {gap}s gap, skipping the only writer of line 1 blamed it on the very \
                     changeset that was skipped"
                ),
            }
        }
    }

    #[test]
    fn skipping_a_revision_leaves_lines_it_did_not_write_alone() {
        // The other half: the fix must not stop attribution working. Line 2 was written by the
        // newer changeset, so ignoring the older one changes nothing about it.
        let conn = two_changesets(5);
        let found = blame_through(&conn, "a.txt", 2, &["csolder".to_string()]).unwrap();
        assert_eq!(
            found.map(|r| r.0).as_deref(),
            Some("csnewer"),
            "a line the skipped revision never wrote lost its author"
        );
    }
}
