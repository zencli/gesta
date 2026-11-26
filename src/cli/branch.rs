//! The branch commands.

use super::*;

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

/// What creating a branch did, so the text and JSON forms describe the same event.
pub struct BranchCreated {
    pub name: String,
    pub from: String,
    pub head: Option<String>,
    /// False when the branch was already there. A create that is already satisfied is not an error,
    /// which is the same answer the text form gave and the reason this is a field rather than an
    /// `Err`.
    pub created: bool,
}

pub fn branch_create(conn: &Connection, name: &str, from: &str) -> ZResult<BranchCreated> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM branches WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(BranchCreated {
            name: name.to_string(),
            from: from.to_string(),
            head: branch_head(conn, name)?,
            created: false,
        });
    }
    let head = branch_head(conn, from)?;
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![name, head, now(), from],
    )?;
    // A new branch points at the same changeset, so it starts with the same
    // committed tree. Without this the branch reads as an empty tree until its
    // first commit, and `status` on it reports every file as new.
    crate::store::refresh_branch_tree(conn, name)?;
    ops::record(conn, "branch create", Some(name), Some(from))?;
    Ok(BranchCreated {
        name: name.to_string(),
        from: from.to_string(),
        head,
        created: true,
    })
}

pub fn cmd_branch_create(name: &str, from: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = branch_create(&conn, name, from)?;
    if !out.created {
        println!("branch {} already exists", out.name);
        return Ok(());
    }
    println!(
        "created branch {} from {} at {}",
        out.name,
        out.from,
        out.head.unwrap_or_else(|| "(empty)".into())
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

/// What switching a branch did.
pub struct BranchSwitched {
    pub current: String,
    pub previous: Option<String>,
    pub head: Option<String>,
    pub written: usize,
    pub removed: usize,
}

pub fn branch_switch(
    conn: &rusqlite::Connection,
    root: &Path,
    name: &str,
) -> ZResult<BranchSwitched> {
    let previous = crate::store::current_branch(conn).ok();
    let target = branch_head(conn, name)?;
    set_state(conn, "current_branch", name)?;
    // Switching branches must make the working tree match the new head, or the
    // next status would report every file as modified.
    let (written, removed) = repo::restore_worktree(conn, root, name)?;
    ops::record(conn, "branch switch", Some(name), None)?;
    // After the tree has moved, so a hook that rebuilds something sees the files it is rebuilding
    // from. A hook that ran before the checkout would read the branch it is leaving.
    repo::hooks::run(root, "post-checkout", &["branch", name])?;
    Ok(BranchSwitched {
        current: name.to_string(),
        previous,
        head: target,
        written,
        removed,
    })
}

pub fn cmd_branch_switch(name: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let out = branch_switch(&conn, &root, name)?;
    println!(
        "switched to branch {} ({} file(s) materialised, {} removed, head {})",
        out.current,
        out.written,
        out.removed,
        out.head.unwrap_or_else(|| "(empty)".into())
    );
    Ok(())
}

/// What deleting a branch did. `head` is the changeset it pointed at, so a caller can name what it
/// removed after the branch is gone.
pub struct BranchDeleted {
    pub name: String,
    pub head: Option<String>,
    pub deleted: bool,
}

pub fn branch_delete(
    conn: &rusqlite::Connection,
    root: &Path,
    name: &str,
) -> ZResult<BranchDeleted> {
    let cur = current_branch(conn)?;
    if name == cur {
        return Err(bad(format!(
            "cannot delete the branch you are on ({})",
            name
        )));
    }
    if branch_head(conn, name).is_err() {
        return Err(bad(format!(
            "no such branch: {}{}",
            name,
            branch_names(conn)
        )));
    }
    // A linked worktree checks this branch out in its own directory. Deleting the branch leaves that
    // directory holding a marker for a branch that no longer exists, and the next command there reads
    // a branch that is gone with no warning at the moment the damage is done. Refusing names the path.
    let holders: Vec<String> = crate::repo::worktree::list(conn, root)?
        .into_iter()
        .filter(|w| w.branch == name && w.path != root)
        .map(|w| w.path.display().to_string())
        .collect();
    if !holders.is_empty() {
        return Err(bad(format!(
            "cannot delete {}: checked out in {}; remove that worktree first with `gesta worktree remove <path>`",
            name,
            holders.join(", ")
        )));
    }
    let head = branch_head(conn, name)?;
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
    ops::record(conn, "branch delete", Some(name), None)?;
    Ok(BranchDeleted {
        name: name.to_string(),
        head,
        deleted: true,
    })
}

pub fn cmd_branch_delete(name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = branch_delete(&conn, &_root, name)?;
    println!("deleted branch {}", out.name);
    Ok(())
}

/// What protecting a branch did.
pub struct BranchProtected {
    pub name: String,
    pub protected: bool,
}

pub fn branch_protect(conn: &Connection, name: &str, unset: bool) -> ZResult<BranchProtected> {
    if crate::store::branch_head(conn, name).is_err() {
        return Err(bad(format!(
            "no such branch: {}{}",
            name,
            branch_names(conn)
        )));
    }
    if !crate::support::approval::set_protected(conn, name, !unset)? {
        return Err(bad(format!("no such branch: {}", name)));
    }
    let protected = !unset;
    Ok(BranchProtected {
        name: name.to_string(),
        protected,
    })
}

pub fn cmd_branch_protect(name: &str, unset: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = branch_protect(&conn, name, unset)?;
    if unset {
        println!(
            "{} is no longer protected; a push to it needs no approval",
            out.name
        );
    } else {
        println!(
            "{} is protected: a push to it now needs a sign-off from a credential other than the one pushing",
            out.name
        );
        println!("sign off a changeset with: gesta changeset approve <id> --by <reviewer>");
    }
    Ok(())
}

pub fn cmd_approve(id: &str, by: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // `optional()?` then a targeted not-found, not `map_err(|_| ...)`: the old form turned every
    // failure - a locked database, a missing column - into "no such changeset", so a caller whose
    // store could not be read was told their changeset did not exist.
    let branch: String = conn
        .query_row(
            "SELECT COALESCE(branch, 'main') FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| bad(format!("no such changeset: {}", id)))?;
    crate::support::approval::approve(&conn, id, &branch, by)?;
    // A sign-off is the event a protected branch turns on, so it is exactly as worth a line as a
    // refused request.
    crate::support::audit::local::approve(&root, id, by, &branch);
    let signers = crate::support::approval::approvers(&conn, id)?;
    println!(
        "{} approved by {} (approvals: {})",
        id,
        by,
        signers.join(", ")
    );
    if crate::support::approval::is_protected(&conn, &branch)? && signers.len() == 1 {
        // Worth saying here rather than at the push: a single approval is enough only if the pusher
        // is a different credential, and that is the thing people get wrong.
        println!(
            "note: {} is protected, so this approval counts only for a push from a different credential than {}",
            branch, by
        );
    }
    Ok(())
}

/// Show who signed off on a changeset.
pub fn cmd_approvals(id: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let signers = crate::support::approval::approvers(&conn, id)?;
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
    emit(crate::support::json::Json::obj(vec![
        (
            "merged",
            crate::support::json::Json::Bool(out.merge_changeset.is_some()),
        ),
        (
            "changeset",
            out.merge_changeset
                .clone()
                .map(crate::support::json::Json::str)
                .unwrap_or(crate::support::json::Json::Null),
        ),
        (
            "applied",
            crate::support::json::Json::arr(
                out.applied
                    .iter()
                    .cloned()
                    .map(crate::support::json::Json::str)
                    .collect(),
            ),
        ),
        (
            "conflicts",
            crate::repo::conflict::reports_to_json(&out.reports),
        ),
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
    emit(crate::repo::conflict::reports_to_json(&out));
    Ok(())
}

pub fn cmd_branch_merge(source: &str, target: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // A `branch merge` is a landing: the change stops being private here. `pre-land` used to run only
    // on the review queue's path through `land_branch`, so a repository that installed the hook and
    // merged directly never had it fire - the guard was listed as installed and did nothing.
    repo::hooks::run(&root, "pre-land", &[source, target])?;
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

/// What info on a branch returned.
pub struct BranchInfo {
    pub name: String,
    pub created_at: i64,
    pub from: Option<String>,
    pub head: Option<String>,
    pub changesets: usize,
    pub protected: bool,
}

pub fn branch_info(conn: &Connection, name: &str) -> ZResult<BranchInfo> {
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
        .map_err(|_| bad(format!("no such branch: {}{}", name, branch_names(conn))))?;

    Ok(BranchInfo {
        name: name.to_string(),
        created_at: ts,
        from,
        head,
        changesets: reachable_from(conn, name)?.len(),
        protected: crate::support::approval::is_protected(conn, name)?,
    })
}

pub fn cmd_branch_info(name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = branch_info(&conn, name)?;
    println!("branch:      {}", out.name);
    println!("created:     {}", fmt_ts(out.created_at));
    println!("from:        {}", out.from.unwrap_or_else(|| "-".into()));
    println!(
        "head:        {}",
        out.head.clone().unwrap_or_else(|| "(empty)".into())
    );
    println!("changesets:  {}", out.changesets);
    println!("protected:   {}", out.protected);
    Ok(())
}

pub fn cmd_branch_json(sub: crate::BranchCmd) -> YResultAlias {
    let (root, conn) = open_repo()?;
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
        // Every branch subcommand answers JSON now, including the ones that write. A mutation under
        // `--json` used to run and print text, so a script that asked for JSON got prose it had to
        // parse - and the argument for "scripting a mutation was never the point" does not survive
        // contact with a CI job that creates a branch and reads the result.
        crate::BranchCmd::Create { name, from } => {
            let name = need_name(
                "branch create needs a name",
                "gesta branch create feature",
                name,
            )?;
            let out = branch_create(&conn, &name, &from)?;
            emit(Json::obj(vec![
                ("schema", Json::str("gesta@1")),
                ("name", Json::str(out.name)),
                ("from", Json::str(out.from)),
                ("head", out.head.map(Json::str).unwrap_or(Json::Null)),
                ("created", Json::Bool(out.created)),
            ]));
            Ok(())
        }
        crate::BranchCmd::Switch { name } => {
            let name = need_name(
                "branch switch needs a name; `gesta branch list` names them",
                "gesta branch switch feature",
                name,
            )?;
            let out = branch_switch(&conn, &root, &name)?;
            emit(Json::obj(vec![
                ("schema", Json::str("gesta@1")),
                ("current", Json::str(out.current)),
                (
                    "previous",
                    out.previous.map(Json::str).unwrap_or(Json::Null),
                ),
                ("head", out.head.map(Json::str).unwrap_or(Json::Null)),
            ]));
            Ok(())
        }
        crate::BranchCmd::Delete { name } => {
            let name = need_name(
                "branch delete needs a name; `gesta branch list` names them",
                "gesta branch delete feature",
                name,
            )?;
            let out = branch_delete(&conn, &root, &name)?;
            emit(Json::obj(vec![
                ("schema", Json::str("gesta@1")),
                ("name", Json::str(out.name)),
                ("deleted", Json::Bool(out.deleted)),
                ("head", out.head.map(Json::str).unwrap_or(Json::Null)),
            ]));
            Ok(())
        }
        crate::BranchCmd::Info { name } => {
            let name = need_name(
                "branch info needs a name; `gesta branch list` names them",
                "gesta branch info main",
                name,
            )?;
            let out = branch_info(&conn, &name)?;
            emit(Json::obj(vec![
                ("schema", Json::str("gesta@1")),
                ("name", Json::str(out.name)),
                ("created_at", Json::int(out.created_at)),
                ("from", out.from.map(Json::str).unwrap_or(Json::Null)),
                ("head", out.head.map(Json::str).unwrap_or(Json::Null)),
                ("changesets", Json::int(out.changesets as i64)),
                ("protected", Json::Bool(out.protected)),
            ]));
            Ok(())
        }
        crate::BranchCmd::Protect { name, unset } => {
            let out = branch_protect(&conn, &name, unset)?;
            emit(Json::obj(vec![
                ("schema", Json::str("gesta@1")),
                ("name", Json::str(out.name)),
                ("protected", Json::Bool(out.protected)),
            ]));
            Ok(())
        }
        crate::BranchCmd::Merge {
            source,
            target,
            author,
        } => {
            let source = need_name(
                "branch merge needs the branch to merge",
                "gesta branch merge feature --target main",
                source,
            )?;
            cmd_branch_merge_json(&source, &target, &author)?;
            Ok(())
        }
        crate::BranchCmd::Conflicts { source, target } => cmd_merge_preview(&source, &target),
    }
}

/// A branch name is required, and clap's message for a missing one says only that a token is absent.
fn need_name(what: &str, example: &str, got: Option<String>) -> ZResult<String> {
    got.ok_or_else(|| bad(format!("{what}, for example `{example}`")))
}

// Text-mode dispatch for the subcommands JSON mode does not cover. Keeping it
// here means `main.rs` does not need to know which subcommands have a JSON form.
pub fn dispatch_branch_text(sub: crate::BranchCmd) -> ZResult<()> {
    // A branch name is the one thing every one of these needs, and clap's message for a missing
    // one says only that a token is absent. Each refusal names the shape and, where it helps, the
    // command that lists what exists: a typo is the common case.
    let need = |what: &str, example: &str, got: Option<String>| -> ZResult<String> {
        got.ok_or_else(|| bad(format!("{what}, for example `{example}`")))
    };
    match sub {
        crate::BranchCmd::Create { name, from } => {
            let name = need(
                "branch create needs a name",
                "gesta branch create feature",
                name,
            )?;
            cmd_branch_create(&name, &from)
        }
        crate::BranchCmd::List => cmd_branch_list(),
        crate::BranchCmd::Switch { name } => {
            let name = need(
                "branch switch needs a name; `gesta branch list` names them",
                "gesta branch switch feature",
                name,
            )?;
            cmd_branch_switch(&name)
        }
        crate::BranchCmd::Delete { name } => {
            let name = need(
                "branch delete needs a name; `gesta branch list` names them",
                "gesta branch delete feature",
                name,
            )?;
            cmd_branch_delete(&name)
        }
        crate::BranchCmd::Merge {
            source,
            target,
            author,
        } => {
            let source = need(
                "branch merge needs the branch to merge",
                "gesta branch merge feature --target main",
                source,
            )?;
            cmd_branch_merge(&source, &target, &author)
        }
        crate::BranchCmd::Info { name } => {
            let name = need(
                "branch info needs a name; `gesta branch list` names them",
                "gesta branch info main",
                name,
            )?;
            cmd_branch_info(&name)
        }
        crate::BranchCmd::Protect { name, unset } => cmd_branch_protect(&name, unset),
        crate::BranchCmd::Conflicts { source, target } => cmd_merge_preview(&source, &target),
    }
}
