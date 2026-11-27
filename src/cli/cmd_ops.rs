//! The ops commands.

use super::*;

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

pub fn cmd_stash(sub: crate::StashCmd, json: bool) -> ZResult<()> {
    use crate::StashCmd;
    match sub {
        StashCmd::Save {
            message,
            author,
            paths,
            hunks,
        } => {
            let (root, conn) = open_repo()?;
            let (id, n) = if hunks.is_empty() {
                crate::repo::stash::save_paths(&conn, &root, &message, &author, &paths)?
            } else {
                crate::repo::stash::save_hunks(&conn, &root, &message, &author, &hunks)?
            };
            if n == 0 {
                println!("nothing to stash: the working tree matches the head");
                return Ok(());
            }
            if json {
                emit(Json::obj(vec![
                    ("id", Json::int(id)),
                    ("files", Json::int(n as i64)),
                    ("message", Json::str(message)),
                ]));
            } else {
                println!("saved stash {} ({} file(s)): {}", id, n, message);
                println!("the working tree is back at the head");
            }
            Ok(())
        }
        StashCmd::List => {
            let (_root, conn) = open_repo()?;
            let all = crate::repo::stash::list(&conn)?;
            if json {
                emit(Json::arr(
                    all.iter()
                        .map(|s| {
                            Json::obj(vec![
                                ("id", Json::int(s.id)),
                                ("message", Json::str(s.message.clone())),
                                ("files", Json::int(s.files.len() as i64)),
                                ("created_at", Json::int(s.created_at)),
                            ])
                        })
                        .collect(),
                ));
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
            let (s, written, blocked) = crate::repo::stash::pop(&conn, &root, id)?;
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
                    let all = crate::repo::stash::list(&conn)?;
                    all.first()
                        .map(|s| s.id)
                        .ok_or_else(|| bad("no stash to drop".to_string()))?
                }
            };
            crate::repo::stash::drop_stash(&conn, target)?;
            println!("dropped stash {}", target);
            Ok(())
        }
    }
}

/// A label short enough for a one-line message, cut on a character boundary.
///
/// The message a person reads names the two ends of the search, and those are branch or tag names,
/// not hex ids: cutting bytes at 14 splits a multi-byte character and panics on the very input the
/// command is supposed to accept. Counting characters costs one pass over at most a few dozen bytes
/// and turns a crash into a label.
fn short_label(s: &str) -> String {
    s.chars().take(14).collect()
}

pub fn cmd_bisect(sub: crate::BisectCmd) -> ZResult<()> {
    use crate::BisectCmd;
    match sub {
        BisectCmd::Start {
            good,
            bad: bad_point,
        } => {
            let (root, conn) = open_repo()?;
            if crate::repo::bisect::load(&conn)?.is_some() {
                return Err(bad(
                    "a bisect is already running: answer it or run `gesta bisect cancel`"
                        .to_string(),
                ));
            }
            let s = crate::repo::bisect::start(&conn, &good, &bad_point)?;
            println!(
                "bisecting {} candidate(s) between {} and {}",
                s.candidates.len(),
                short_label(&good),
                short_label(&bad_point)
            );
            // The first question comes right away, so the caller never has to
            // make a separate "what next" call before testing anything.
            match crate::repo::bisect::step(&conn, &root, true)? {
                crate::repo::bisect::Step::Test(id) => {
                    println!("testing {}", id);
                    println!("the working tree now holds that changeset: run your test, then");
                    println!("  gesta bisect good    if it passes");
                    println!("  gesta bisect bad     if it fails");
                }
                crate::repo::bisect::Step::Found(id) => {
                    crate::repo::bisect::finish(&conn, &root)?;
                    println!("first bad changeset: {}", id);
                }
            }
            Ok(())
        }
        BisectCmd::Run { command } => {
            let (root, conn) = open_repo()?;
            if crate::repo::bisect::load(&conn)?.is_none() {
                return Err(bad(
                    "no bisect in progress (run `gesta bisect start`)".to_string()
                ));
            }
            // Each answer is printed as it is decided, because the interesting part of an automated
            // search is not the answer at the end but which changeset first failed and why.
            let answer = crate::repo::bisect::run(&conn, &root, &command, |verdict, id| {
                println!("{verdict} {}", &id[..id.len().min(14)]);
            })?;
            println!("first bad changeset: {answer}");
            println!("the working tree is back at the branch head");
            if let Ok(Some((msg, author))) = conn
                .query_row(
                    "SELECT message, author FROM changesets WHERE id = ?1",
                    rusqlite::params![answer],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .map(Some)
            {
                println!("  {} by {}", msg, author);
            }
            Ok(())
        }
        BisectCmd::Good | BisectCmd::Bad => {
            let is_good = matches!(sub, BisectCmd::Good);
            let (root, conn) = open_repo()?;
            match crate::repo::bisect::step(&conn, &root, is_good)? {
                crate::repo::bisect::Step::Test(id) => {
                    println!("testing {}", id);
                    Ok(())
                }
                crate::repo::bisect::Step::Found(id) => {
                    // Restoring the tree is part of finishing, not a separate
                    // thing to remember.
                    crate::repo::bisect::finish(&conn, &root)?;
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
            match crate::repo::bisect::load(&conn)? {
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
            if crate::repo::bisect::load(&conn)?.is_none() {
                println!("no bisect running");
                return Ok(());
            }
            let n = crate::repo::bisect::finish(&conn, &root)?;
            println!("bisect cancelled, {} file(s) restored to the head", n);
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
        let name = crate::repo::rebase::abort(&conn, &root)?;
        println!("rebase aborted, {} is back where it started", name);
        return Ok(());
    }

    if do_continue {
        report_rebase(crate::repo::rebase::continue_after_resolution(
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
    // The tree the replay starts from is the one the branch is moving onto, not the branch's own
    // previous head. A changeset on the branch may have deleted the file long before the rebase,
    // and comparing against the branch's own past would then see nothing leave the tree.
    let onto_head = crate::store::resolve_rev(&conn, onto)?;
    let progress = crate::repo::rebase::start(&conn, &root, &target, onto, author)?;
    let after = crate::store::branch_head(&conn, &target)?;
    let (dangling, unchecked) = match &progress {
        // Only a finished replay can carry this: a stopped one will report on the run that
        // finishes it, and reporting twice would show the same names again.
        crate::repo::rebase::Progress::Done(_) => crate::repo::rebase::dangling_after_with_skips(
            &conn,
            onto_head.as_deref(),
            after.as_deref().unwrap_or(""),
        )
        .unwrap_or_default(),
        _ => (Vec::new(), Vec::new()),
    };
    report_rebase(progress);
    report_dangling(&dangling);
    report_unchecked(&unchecked);
    Ok(())
}

/// Files the dangling check could not read.
///
/// Its own message rather than part of the report above: "no references found" and "no references
/// found in the files I could read" are different answers, and the second one has to say so or the
/// reader trusts a result that was never fully computed.
fn report_unchecked(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    println!();
    println!(
        "{} file(s) could not be read, so the check above did not cover them:",
        paths.len()
    );
    for p in paths {
        println!("  {}", p);
    }
    println!();
    println!("the store cannot produce these bodies; `gesta fsck` reports the damage.");
}

/// Names that left the tree while other files still mention them.
///
/// Silence is the normal case, so anything printed here has to be worth interrupting for. It is
/// not an error and the rebase succeeded: the tree holds a reference to a path that is not in it,
/// which is a fact about the result rather than a failure of the operation.
fn report_dangling(found: &[crate::repo::dangling::Dangling]) {
    if found.is_empty() {
        return;
    }
    println!();
    println!(
        "{} reference(s) to a path that is no longer in the tree:",
        found.len()
    );
    for d in found {
        println!("  {}:{} names {}", d.used_by, d.line, d.removed);
        if !d.text.is_empty() {
            println!("    {}", d.text);
        }
    }
    println!();
    println!("the rebase succeeded; this is what it left behind. Fix the line, or remove it.");
}

fn report_rebase(p: crate::repo::rebase::Progress) {
    use crate::repo::rebase::Progress;
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

/// `gesta worktree` - additional working trees sharing this store.
pub fn cmd_worktree(sub: crate::WorktreeCmd) -> ZResult<()> {
    use crate::WorktreeCmd;
    let (root, conn) = open_repo()?;
    match sub {
        WorktreeCmd::Add { path, branch } => {
            // Both arguments are required in meaning, and neither is guessable: a path with no
            // branch would have to pick one, and a branch with no path has nowhere to go. The
            // refusal names the shape, because clap's message only says which token is missing.
            let path = path.ok_or_else(|| {
                bad(
                    "worktree add needs a path and a branch: `gesta worktree add ../other feature`"
                        .to_string(),
                )
            })?;
            let branch = branch.ok_or_else(|| {
                bad(format!(
                    "worktree add needs a branch as well as a path: `gesta worktree add {} <branch>`",
                    path
                ))
            })?;
            let target = std::path::PathBuf::from(&path);
            let target = if target.is_absolute() {
                target
            } else {
                root.join(target)
            };
            crate::repo::worktree::add(&conn, &root, &target, &branch)
        }
        WorktreeCmd::List => {
            let trees = crate::repo::worktree::list(&conn, &root)?;
            for (i, t) in trees.iter().enumerate() {
                // The tree holding the store is marked, because removing it is not a thing this
                // command does and a list that does not say which one it is invites the attempt.
                let tag = if i == 0 { " (main)" } else { "" };
                println!("{} [{}]{}", t.path.display(), t.branch, tag);
            }
            Ok(())
        }
        WorktreeCmd::Remove { path, force } => {
            let path = path.ok_or_else(|| {
                bad(
                    "worktree remove needs the tree to remove: `gesta worktree list` names them"
                        .to_string(),
                )
            })?;
            let target = std::path::PathBuf::from(&path);
            let target = if target.is_absolute() {
                target
            } else {
                root.join(target)
            };
            // Removing the tree that holds the store would delete the repository out from under
            // itself. That is never what was meant, so it is refused rather than guessed at.
            if std::fs::canonicalize(&target).ok() == std::fs::canonicalize(&root).ok() {
                return Err(bad(
                    "that is the main working tree, which holds the store; remove another tree"
                        .to_string(),
                ));
            }
            crate::repo::worktree::remove(&conn, &target, force)
        }
    }
}

/// `gesta notes` - annotations attached to a changeset, kept apart from it.
pub fn cmd_notes(sub: crate::NotesCmd) -> ZResult<()> {
    use crate::NotesCmd;
    let (_root, conn) = open_repo()?;
    match sub {
        NotesCmd::Add { changeset, message } => {
            // The author of the note is the author of the changeset it annotates (#336).
            //
            // It used to be `get_state(&conn, "user_name")`, which nothing in the program ever writes,
            // so the column was always the literal "unknown" and `notes show` printed `  -- unknown`
            // under every note. The changeset's own author is the person who made the change being
            // explained, which is who a note is from, and it is a value that actually exists.
            let author: String = conn
                .query_row(
                    "SELECT author FROM changesets WHERE id = ?1",
                    rusqlite::params![changeset],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_else(|| "unknown".to_string());
            crate::repo::notes::add(&conn, &changeset, &message, &author)?;
            println!("noted on {changeset}");
            Ok(())
        }
        NotesCmd::Show { changeset } => match crate::repo::notes::show(&conn, &changeset)? {
            Some((body, author)) => {
                println!("{body}");
                // Printed only when it carries a name: an old row with an empty author used to render a
                // bare `  -- ` line, which reads as a truncation bug rather than as "no author".
                if !author.trim().is_empty() {
                    println!("  -- {author}");
                }
                Ok(())
            }
            None => Err(bad(format!("no note on {changeset}"))),
        },
        NotesCmd::Remove { changeset } => {
            if crate::repo::notes::remove(&conn, &changeset)? {
                println!("removed the note on {changeset}");
                println!("the changeset itself is unchanged");
                Ok(())
            } else {
                Err(bad(format!("no note on {changeset}")))
            }
        }
    }
}

#[cfg(test)]
mod label_tests {
    use super::short_label;

    #[test]
    fn a_long_revision_name_is_cut_without_panicking() {
        // `é` is two bytes, so thirteen ASCII bytes followed by it put byte 14 inside that
        // character. The panic this replaces was a crash on an ordinary branch name, not an invalid
        // one, and it could happen after the session row was already written.
        let name = format!("{}\u{e9}", "a".repeat(13));
        let label = short_label(&name);
        assert_eq!(label.chars().count(), 14);
        // The whole character survives: a byte cut would have produced "aaaaaaaaaaaaa" plus a
        // half-written é, or panicked before producing anything.
        assert_eq!(label, format!("{}\u{e9}", "a".repeat(13)));
    }

    #[test]
    fn a_short_name_is_kept_whole() {
        assert_eq!(short_label("main"), "main");
        assert_eq!(short_label(""), "");
    }
}
