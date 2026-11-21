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
        StashCmd::Save { message, author } => {
            let (root, conn) = open_repo()?;
            let (id, n) = crate::repo::stash::save(&conn, &root, &message, &author)?;
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
            let all = crate::repo::stash::list(&conn)?;
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
                &good[..good.len().min(14)],
                &bad_point[..bad_point.len().min(14)]
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
    let progress = crate::repo::rebase::start(&conn, &root, &target, onto, author)?;
    report_rebase(progress);
    Ok(())
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
