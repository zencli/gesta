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

pub fn cmd_changeset_commit(id: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // Before the hook and before anything is written: a refusal here leaves the repository exactly
    // as it was, which is the whole point of checking at this moment.
    refuse_secrets(&conn, &root, id)?;
    // The hook runs before anything is written, so a refusal leaves the
    // repository exactly as it was. A hook that ran afterwards could only report
    // a commit that already happened.
    crate::repo::hooks::run(&root, "pre-commit", &["commit", id])?;
    let n = store::with_retry(|| repo::commit_one(&conn, id))?;
    if n > 0 {
        // A post-commit failure is reported but does not undo the commit: the
        // commit is durable, and rolling it back here would leave the store and
        // the message disagreeing. The error still propagates so a caller knows.
        crate::repo::hooks::run(&root, "post-commit", &["commit", id])?;
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
        crate::support::audit::local::commit(&root, &branch, id, "local", n);
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
