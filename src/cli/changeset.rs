//! The changeset commands.

use super::*;

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

    // Before the snapshot, not after: a hook that refuses a tree has to refuse it while refusing costs
    // nothing. This is also the only point where a hook can see what is about to be recorded rather
    // than what already was.
    repo::hooks::run(&root, "pre-changeset", &["changeset", &branch])?;

    let snapshot =
        store::with_retry(|| repo::create_changeset_scoped(&conn, &root, message, author, paths))?;
    let outcome = match &snapshot {
        Some(cs) => SnapshotOutcome::Created(cs.id.clone()),
        None => SnapshotOutcome::Nothing,
    };
    create_outcome(&branch, outcome)?;

    let Some(cs) = snapshot else {
        // `create_outcome` returned the error for this case, so reaching here is impossible today.
        // Kept as a plain return rather than `unreachable!` so a future change to `create_outcome`
        // degrades into "no output" instead of a panic in the middle of a repository operation.
        return Ok(());
    };
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

/// What a snapshot request produced, and how the command reports it.
///
/// Split from the command so a test can drive the decision against a real repository: the command
/// opens the repository from the working directory, which a test process cannot change safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotOutcome {
    Created(String),
    Nothing,
}

/// The status a snapshot outcome reports, and the one line that changed for #338.
///
/// `Nothing` used to print a note and return `Ok(())`. A caller running twenty parallel `create` +
/// `commit-all` pairs got exit 0 from all forty commands while only two of twenty changesets existed,
/// because another writer had already committed the files a `create` was about to snapshot. The note
/// went to stdout and the status said success, so the only way to notice was to count `log`
/// afterwards. `Nothing` keeps its message and gains exit 1.
pub(crate) fn create_outcome(branch: &str, outcome: SnapshotOutcome) -> ZResult<()> {
    match outcome {
        SnapshotOutcome::Created(_) => Ok(()),
        SnapshotOutcome::Nothing => Err(Box::new(crate::error::Error::new(
            crate::error::Code::Other,
            format!(
                "nothing to snapshot: working tree matches branch {}",
                branch
            ),
        ))),
    }
}

/// The changeset a commit names, or the only pending one when the caller named none.
///
/// A session usually has exactly one pending changeset, and requiring its id there costs a round
/// trip whose whole content is a value the program already knows. With several pending, the id
/// stays required: picking one silently would commit work the caller did not name.
fn resolve_commit_id(conn: &rusqlite::Connection, id: Option<&str>) -> ZResult<String> {
    if let Some(id) = id {
        return Ok(id.to_string());
    }
    let pending: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    match pending.len() {
        0 => Err(bad(
            "no pending changeset to commit: make one with `gesta changeset create`".to_string(),
        )),
        1 => Ok(pending[0].clone()),
        n => Err(bad(format!(
            "{} pending changesets, so the id is required: {}",
            n,
            pending.join(", ")
        ))),
    }
}

/// Commit one changeset through everything a commit has to pass.
///
/// There were five paths that commit a changeset and four of them called `commit_one` directly, so
/// the secret scanner, both hooks, the undo record and the audit line existed only on the text form.
/// The scanner is a security gate, and the form agents use was the one without it - a credential in a
/// file could be committed through `--json`, `apply`, `commit-all` or `brief --fix` and nothing
/// printed or logged a word. `changeset.rs:99` was the reference path, so the five steps are lifted
/// here and every caller goes through this function; adding a sixth caller cannot skip them by
/// forgetting.
///
/// Returns the number of files the commit moved, which is 0 when the changeset was already
/// committed. The two hooks still run in that case, because they are the caller's answer to "a commit
/// is about to happen", and only the commit itself is idempotent.
pub(crate) fn commit_checked(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    id: &str,
) -> ZResult<usize> {
    // Before the hook and before anything is written: a refusal here leaves the repository exactly
    // as it was, which is the whole point of checking at this moment.
    refuse_secrets(conn, root, id)?;
    // The hook runs before anything is written, so a refusal leaves the repository exactly as it
    // was. A hook that ran afterwards could only report a commit that already happened.
    crate::repo::hooks::run(root, "pre-commit", &["commit", id])?;
    let n = store::with_retry(|| repo::commit_one(conn, id))?;
    if n > 0 {
        // A post-commit failure is reported but does not undo the commit: the commit is durable, and
        // rolling it back here would leave the store and the message disagreeing. The error still
        // propagates so a caller knows.
        crate::repo::hooks::run(root, "post-commit", &["commit", id])?;
        // Recorded after the fact with the state it produced, because undo restores an operation's
        // own snapshot and a commit has no earlier hook here. The important part is that `undo` and
        // this agree on the meaning; see `ops::record`.
        ops::record(conn, "commit", Some(id), None)?;
        // The audit record held only what the server decided about requests. A commit is the same
        // kind of fact, and an operator reading the log saw arrivals and nothing about what this
        // machine did with them.
        let branch = current_branch(conn)?;
        crate::support::audit::local::commit(root, &branch, id, "local", n);
    }
    Ok(n)
}

pub fn cmd_changeset_commit(id: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let id = resolve_commit_id(&conn, id)?;
    let id = id.as_str();
    let n = commit_checked(&conn, &root, id)?;
    if n == 0 {
        println!("changeset {} already committed", id);
    } else {
        let branch = current_branch(&conn)?;
        println!("committed {} on branch {} ({} file(s))", id, branch, n);
    }
    Ok(())
}

pub fn cmd_changeset_apply(id: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let cutoff: Option<i64> = conn
        .query_row(
            "SELECT created_at FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    let cutoff = cutoff.ok_or_else(|| {
        crate::support::error::Error::not_found(format!("no such changeset: {}", id))
    })?;

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
    // Each changeset passes the same gate as a single commit. Applying a run of changesets is not a
    // reason to skip the scanner for the ones in the middle.
    let mut total = 0usize;
    for cs in &ids {
        total += commit_checked(&conn, &root, cs)?;
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
    let (root, conn) = open_repo()?;
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
        total += commit_checked(&conn, &root, cs)?;
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
        msg.ok_or_else(|| crate::support::error::Error::not_found(format!(
            "no such changeset: {}",
            id
        )))?
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

/// Print the changeset that holds a git commit.
///
/// Matched on a prefix rather than the whole id: a person reading `git log` has the short form, and
/// asking them to fetch the long one is the reason a lookup gets skipped.
/// The changeset holding the git commit whose id starts with `prefix`.
///
/// Split from the command so a test can call it: `open_repo` reads the working directory, and a test
/// that had to change directory to exercise a lookup would be testing the test.
///
/// The match is an exact prefix, so the input is compared with `substr`, not fed to `LIKE`. A `LIKE`
/// pattern lets `%` and `_` in the input act as wildcards, and `find --commit %` then answers with an
/// arbitrary changeset instead of saying nothing matched. Query errors propagate for the same reason:
/// `.ok()` turned a broken database into "no such commit", which is a different statement.
pub fn changeset_for_commit(conn: &Connection, prefix: &str) -> ZResult<Option<(String, String)>> {
    let len = prefix.chars().count() as i64;
    let row = conn
        .query_row(
            "SELECT id, COALESCE(git_commit, '') FROM changesets
             WHERE git_commit IS NOT NULL AND substr(git_commit, 1, ?1) = ?2
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
            rusqlite::params![len, prefix],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row)
}

pub fn cmd_changeset_for_commit(commit: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    match changeset_for_commit(&conn, commit)? {
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
        crate::ChangesetCmd::Commit { id } => cmd_changeset_commit_json(id.as_deref()),
        // A caller that asked for JSON and got text has a silent failure: it parses nothing and
        // branches on nothing, and the run looks successful. Refuse rather than print prose.
        _other => Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Usage,
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
    // The same hook the text path runs. It was missing here, so `changeset create --json` - the form
    // every agent uses, and the base of every bench in this repository - skipped `pre-changeset`
    // entirely while the hook was listed as installed.
    repo::hooks::run(&root, "pre-changeset", &["changeset", &branch])?;
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
fn cmd_changeset_commit_json(id: Option<&str>) -> YResultAlias {
    let (root, conn) = open_repo()?;
    let id = resolve_commit_id(&conn, id)?;
    let id = id.as_str();
    // The JSON form is the form agents use, so it is the path a credential is most likely to travel.
    // It used to be the one without the scanner.
    let written = commit_checked(&conn, &root, id)?;
    emit(Json::obj(vec![
        ("id", Json::str(id)),
        ("files", Json::int(written as i64)),
        ("already_committed", Json::Bool(written == 0)),
    ]));
    Ok(())
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

#[cfg(test)]
mod commit_gate_tests {
    use super::*;

    /// A repository with one pending changeset that adds `a.txt`.
    fn repo_with_pending(name: &str) -> (std::path::PathBuf, rusqlite::Connection, String) {
        let d = std::env::temp_dir().join(format!("zf-gate-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::tests::mem_db();
        std::fs::write(d.join("a.txt"), "hello\n").unwrap();

        let cs = crate::repo::create_changeset(&conn, &d, "m", "t")
            .unwrap()
            .expect("a fresh file is a new changeset");
        (d, conn, cs.id)
    }

    /// A hook that always fails, written in whatever the platform can actually execute.
    ///
    /// The first version was `#!/bin/sh` with `exit 7`, which is right on Linux and produced
    /// `%1 is not a valid Win32 application (os error 193)` on Windows, where this test then failed
    /// for a reason that had nothing to do with the commit gate. A test that passes on one platform
    /// has not been run on the other.
    fn always_failing_hook() -> String {
        if cfg!(windows) {
            // A batch file, because cmd.exe is the interpreter Windows always has. The code has to
            // reach the caller the same way `exit 7` does on Unix.
            "@echo off\r\nexit /b 7\r\n".to_string()
        } else {
            "#!/bin/sh\nexit 7\n".to_string()
        }
    }

    #[test]
    fn a_failing_pre_commit_hook_stops_every_commit_path() {
        // #182: only the text form ran the hook. `--json`, `apply`, `commit-all` and `brief --fix`
        // called `commit_one` directly, so a repository could refuse a commit from the shell and
        // accept the same commit from an agent.
        let (d, conn, id) = repo_with_pending("hook");
        crate::repo::hooks::install(&d, "pre-commit", &always_failing_hook()).unwrap();

        let err = commit_checked(&conn, &d, &id).unwrap_err().to_string();
        assert!(
            err.contains('7'),
            "the hook's code must be reported, got {err}"
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM changesets WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "pending",
            "a refused commit must leave the changeset pending"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_passing_commit_records_an_audit_line() {
        // The audit log existed for the server's decisions and was written by the text form only.
        // A commit made through any other path left no trace of what this machine did.
        let (d, conn, id) = repo_with_pending("audit");
        let n = commit_checked(&conn, &d, &id).unwrap();
        assert!(n > 0, "the changeset should have committed");
        let log = std::fs::read_to_string(d.join(".gesta/audit.log")).unwrap_or_default();
        assert!(
            log.contains(&id),
            "the commit must appear in the audit log, got {log:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_committing_command_goes_through_the_gate() {
        // The point of #182 is not one missing call but four. A check on the source text is the only
        // way to keep a sixth caller from being added later without the gate; the compiler cannot
        // tell `commit_one` from `commit_checked`.
        //
        // The first version of this test skipped every line after the word `commit_checked` appeared,
        // which was on line 100, so it examined almost nothing and passed with the bug put back. It
        // now looks at the body of each function separately: `commit_checked` is the one place
        // allowed to call `commit_one`.
        let src = include_str!("changeset.rs");
        // Only the code above the test module; the test's own source quotes the pattern it looks for.
        let src = &src[..src.find("mod commit_gate_tests").unwrap()];
        let lines: Vec<&str> = src.lines().collect();
        let mut current = String::new();
        let mut depth = 0i32;
        let mut loose: Vec<(usize, String, String)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if depth == 0 {
                if let Some(rest) = trimmed
                    .strip_prefix("fn ")
                    .or_else(|| trimmed.strip_prefix("pub fn "))
                {
                    current = rest.split('(').next().unwrap_or("").to_string();
                } else if let Some(pos) = trimmed.find(" fn ") {
                    current = trimmed[pos + 4..]
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .to_string();
                }
            }
            if line.contains("repo::commit_one(") && current != "commit_checked" {
                loose.push((i + 1, current.clone(), trimmed.to_string()));
            }
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        }
        assert!(
            loose.is_empty(),
            "a commit path bypasses commit_checked: {loose:?}"
        );
        // And the gate really is the one that runs the five steps.
        let start = src.find("pub(crate) fn commit_checked").unwrap();
        // Up to the doc comment of the next item: brace counting would work too, but this text is
        // short and the boundary is unambiguous.
        let body = &src[start..start + src[start..].find("pub fn cmd_changeset_commit(").unwrap()];
        for step in [
            "refuse_secrets",
            "\"pre-commit\"",
            "repo::commit_one",
            "\"post-commit\"",
            "ops::record",
            "audit::local::commit",
        ] {
            assert!(body.contains(step), "commit_checked must run {step}");
        }
    }
}

#[cfg(test)]
mod find_tests {
    use super::*;

    fn two_commits() -> Connection {
        let conn = crate::store::tests::mem_db();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, branch, git_commit)
             VALUES('cs1', 'first', 'a', 0, 'committed', 'main', 'aaaabbbbccccdddd1111')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, branch, git_commit)
             VALUES('cs2', 'second', 'a', 1, 'committed', 'main', 'bbbbccccddddeeee2222')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_wildcard_in_the_input_matches_nothing_instead_of_everything() {
        let conn = two_commits();
        // `LIKE '%%'` matched every row, so the command answered with an arbitrary changeset.
        assert_eq!(changeset_for_commit(&conn, "%").unwrap(), None);
        assert_eq!(changeset_for_commit(&conn, "_").unwrap(), None);
        assert_eq!(changeset_for_commit(&conn, "a%b").unwrap(), None);
    }

    #[test]
    fn a_real_prefix_still_finds_its_commit() {
        let conn = two_commits();
        let (id, full) = changeset_for_commit(&conn, "aaaabbbb").unwrap().unwrap();
        assert_eq!(id, "cs1");
        assert_eq!(full, "aaaabbbbccccdddd1111");
        assert_eq!(
            changeset_for_commit(&conn, "aaaabbbbccccdddd1111")
                .unwrap()
                .unwrap()
                .0,
            "cs1"
        );
    }

    #[test]
    fn a_prefix_that_matches_nothing_is_none_not_a_wildcard_hit() {
        let conn = two_commits();
        assert_eq!(changeset_for_commit(&conn, "ffff").unwrap(), None);
    }

    #[test]
    fn the_lookup_is_anchored_at_the_start() {
        let conn = two_commits();
        // `substr(git_commit, 1, n) = prefix` is anchored at the front, so a middle fragment misses.
        assert_eq!(
            changeset_for_commit(&conn, "bbbbcccc").unwrap().unwrap().0,
            "cs2"
        );
        assert_eq!(changeset_for_commit(&conn, "ccccdddd").unwrap(), None);
    }
    #[test]
    fn a_create_with_nothing_to_snapshot_reports_failure() {
        // #338: a `create` that snapshotted nothing used to print a note and exit 0. Twenty parallel
        // `create` + `commit-all` pairs reported forty successes and left two changesets. Measured on
        // the bug: 18, 0 and 16 of twenty creates reported failure once the status was fixed,
        // matching 2, 20 and 4 changesets in the log.
        //
        // `create_outcome` is the decision the command makes, called here directly: the command opens
        // the repository from the working directory, which a test process cannot change safely.
        let e = create_outcome("main", SnapshotOutcome::Nothing)
            .expect_err("nothing to snapshot must not report success");
        assert!(
            e.to_string().contains("nothing to snapshot"),
            "the message must still say what happened, got: {e}"
        );
        assert!(
            e.to_string().contains("main"),
            "and which branch it matched, got: {e}"
        );

        // The control: a snapshot that produced a changeset is a success, so the guard cannot pass by
        // refusing every call.
        create_outcome("main", SnapshotOutcome::Created("cs1234".to_string()))
            .expect("a created changeset must be reported as success");
    }

    #[test]
    fn a_real_repository_with_no_change_yields_no_snapshot() {
        // The condition `create_outcome` reports on, produced by the real snapshot function rather
        // than asserted about a value this test made up.
        let dir = std::env::temp_dir().join(format!("gesta-cs-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&dir).unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, ?1, NULL)",
            rusqlite::params![0i64],
        )
        .unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();

        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        let cs = crate::repo::create_changeset(&conn, &dir, "one", "alice")
            .unwrap()
            .expect("a written file is something to snapshot");
        crate::repo::commit_one(&conn, &cs.id).unwrap();

        let nothing =
            crate::repo::create_changeset_scoped(&conn, &dir, "nothing", "alice", &[]).unwrap();
        assert!(
            nothing.is_none(),
            "the tree matches the branch, so there is nothing to snapshot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
