//! The tree commands.

use super::*;

/// `sparse set|list|clear`.
pub fn cmd_sparse(sub: crate::SparseCmd) -> ZResult<()> {
    match sub {
        crate::SparseCmd::List => {
            let (_root, conn) = open_repo()?;
            let scope = crate::store::sparse::SparseScope::load(&conn)?;
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
            let scope = crate::store::sparse::SparseScope {
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
            crate::store::sparse::SparseScope::clear(&conn)?;
            let branch = current_branch(&conn)?;
            let n = repo::materialise(&conn, &root, &branch)?;
            println!("working set: everything. {} file(s) restored.", n);
            Ok(())
        }
    }
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

    let report = crate::support::privacy::rewrite_authors(&conn, branch, authors, to)?;
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

    let scope = crate::store::sparse::SparseScope::load(&conn)?;
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
                // A file that is not text is still a change. Skipping it here - before `any` was set -
                // made a tree whose only edits were binary print "no changes", while `status` listed
                // the same files as modified: the two commands disagreed about whether the tree was
                // clean, and `diff` is the one people trust before a commit.
                let content = match walk::read_text(&full) {
                    Some(text) => text,
                    None => {
                        any = true;
                        if name_only {
                            println!("{}", rel);
                        } else {
                            println!("\n--- {}", rel);
                            println!("binary files differ");
                        }
                        continue;
                    }
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
                // Hunks, numbered, because a number is what `stash save --hunk` takes. `line_diff`
                // reports every change in a file as one block, which is right for reading and
                // useless for selecting: two edits a hundred lines apart came out as one hunk and
                // there was no way to name either of them.
                let old_lines: Vec<&str> = old_text.lines().collect();
                let new_lines: Vec<&str> = content.lines().collect();
                let hunks = merge::diff_hunks(&old_text, &content);
                for (i, h) in hunks.iter().enumerate() {
                    println!("@{}({}) ", i + 1, rel);
                    for l in old_lines.iter().skip(h.old_start).take(h.old_len) {
                        println!("-{}", l);
                    }
                    for l in new_lines.iter().skip(h.new_start).take(h.new_len) {
                        println!("+{}", l);
                    }
                }
            }
            if !any {
                println!("no changes");
            }
        }
    }
    Ok(())
}

pub fn cmd_revert(id: &str, reason: Option<&str>, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let outcome =
        store::with_retry(|| history::revert_changeset(&conn, &root, id, author, reason))?;
    let revert_id = outcome.id;
    ops::record(&conn, "revert", Some(&revert_id), Some(id))?;
    println!("reverted {} as {}", id, revert_id);
    println!("\nThe revert is a normal changeset: `gesta log` shows both, and");
    println!("reverting the revert restores the change.");
    // Said after the good news, and without the word "failed": the revert is in the history and the
    // branch points at it. Only the disk disagrees, and the caller needs to know that before they
    // trust the working tree they are about to read.
    if let Some(problem) = outcome.materialise_error {
        println!();
        println!("but the working tree did not follow: {}", problem);
        println!(
            "the history is correct; `gesta checkout {}` writes the files again.",
            revert_id
        );
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
    emit(crate::support::json::Json::obj(vec![
        (
            "files",
            crate::support::json::Json::arr(
                entries
                    .iter()
                    .map(|(p, h)| {
                        crate::support::json::Json::obj(vec![
                            ("path", crate::support::json::Json::str(p.clone())),
                            ("hash", crate::support::json::Json::str(h.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "count",
            crate::support::json::Json::int(entries.len() as i64),
        ),
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
                crate::support::error::Error::not_found(format!(
                    "no such revision: {}. A revision may be HEAD, a branch, a tag, a changeset id, or an unambiguous prefix of one.",
                    r
                ))
            })?,
        None => {
            let branch = current_branch(conn)?;
            crate::store::branch_head(conn, &branch)?.ok_or_else(|| {
                Box::new(crate::support::error::Error::new(
                    crate::support::error::Code::NotFound,
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
                return Err(crate::support::error::Error::new(
                    crate::support::error::Code::Usage,
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
                // Three different questions land here and each needs its own sentence.
                //
                // With a colon the reader named a revision and it does not resolve, so the message
                // is about the revision. Without a colon the name was read as a path against the
                // current branch, so the message is about the file - and a repository with no
                // changesets has no tree to look in, which is a third case again. Naming the wrong
                // one sends the reader to look at the thing that was never in question.
                let branch = current_branch(&conn)?;
                let empty = crate::store::branch_head(&conn, &branch)?.is_none();
                let hint = if named_path {
                    format!(
                        "no such branch, tag or changeset: {}. A revision may be HEAD, a branch, a tag, a changeset id, or an unambiguous prefix of one",
                        rev
                    )
                } else if empty {
                    format!(
                        "branch {} has no changesets yet, so there is nothing to show; make one with `gesta changeset create` and `gesta changeset commit`",
                        branch
                    )
                } else {
                    format!(
                        "{} was read as a file, and no file by that name is at {}; to read a path from another revision, write `gesta show <revision>:{}`",
                        rev, branch, rev
                    )
                };
                return Err(crate::support::error::Error::not_found(hint).into());
            }
        },
    };
    let hash = hash_at_point(&conn, &path, &cs)?.ok_or_else(|| {
        // Without a colon, the name the reader typed went in as a path against the current branch.
        // Saying so here is what separates "this file is not in the tree" from "this branch does
        // not exist", which the bare `does not exist at main` leaves the reader to guess between.
        let message = if named_path {
            format!("{} does not exist at {}", path, rev)
        } else {
            // A hint that contains `<file>` is a template, and a reader who pastes it gets a shell
            // redirect. Both forms are spelled out instead, with the name they actually typed.
            format!(
                "{} was read as a file, and no file by that name is at {}; to read a path from another revision, write `gesta show <revision>:{}`",
                path, rev, path
            )
        };
        crate::support::error::Error::new(crate::support::error::Code::PathNotFound, message)
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
            ("path", crate::support::json::Json::str(path.clone())),
            ("rev", crate::support::json::Json::str(rev.clone())),
            ("changeset", crate::support::json::Json::str(cs.clone())),
            ("hash", crate::support::json::Json::str(hash.clone())),
            ("size", crate::support::json::Json::int(bytes.len() as i64)),
            ("binary", crate::support::json::Json::Bool(text.is_none())),
        ];
        match text {
            Some(t) => fields.push(("content", crate::support::json::Json::str(t))),
            None => fields.push((
                "content_base64",
                crate::support::json::Json::str(crate::transport::remote::enc_base64(&bytes)),
            )),
        }
        emit_object(fields);
        return Ok(());
    }
    use std::io::Write;
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}

pub fn cmd_checkout(file: Option<&str>, changeset: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // `checkout` writes the branch out, so the whole-tree form is what the command means and it
    // cannot lose uncommitted work - the branch already holds every file it writes. A named file
    // stays available for the narrower question.
    if let Some(cs) = changeset {
        // An explicit changeset means: restore that changeset's tree, regardless of whether a
        // file was also given. The old code ignored `changeset` when no file was provided, so
        // `gesta checkout --changeset cs123` silently restored the branch head instead of cs123.
        let written = repo::restore_at_changeset(&conn, &root, cs)?;
        println!("restored {} file(s) from changeset {}", written, cs);
        return Ok(());
    }
    let file = file.unwrap_or(".");
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

/// Replace this repository with a backup.
///
/// The destination is the repository this command is run inside, and the old store is kept as
/// `<db>.before-restore` until the new one has opened, so a failure puts it back.
pub fn cmd_restore_repo(file: &str, yes: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let path = std::path::PathBuf::from(file);

    // A sealed backup is unsealed to a private temporary file first (#340).
    //
    // `backup` now writes a sealed container, so this command reads one. It used to open the named
    // file directly as SQLite, which was correct only while backups were plain - and a sealed file
    // handed to SQLite is "file is not a database", which is the message a person sees while trying to
    // recover from a backup they cannot read.
    //
    // The detection is the magic, not a flag: a file that starts with `gesta-backup-v1` is sealed and
    // needs a passphrase, and an older plain backup still restores without one.
    let named = path.clone();
    let raw = std::fs::read(&path)?;
    let sealed = raw.starts_with(crate::support::backup_remote::MAGIC.as_bytes());
    let _keep_alive;
    let path = if sealed {
        let pass = match crate::cli::secret_not_in_argv(
            None,
            "GESTA_BACKUP_PASSPHRASE",
            "backup passphrase: ",
            false,
        ) {
            Ok(p) => p,
            Err(e) => {
                return Err(Box::new(crate::support::error::Error::new(
                    crate::support::error::Code::Usage,
                    format!("this backup is sealed, so restoring it needs its passphrase: {e}"),
                )));
            }
        };
        let plain = crate::support::backup_remote::unseal(&raw, &pass)?;
        let temp = std::env::temp_dir().join(format!(
            "gesta-restore-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        crate::support::private_file::write_private(&temp, &plain)?;
        _keep_alive = temp.clone();
        temp
    } else {
        _keep_alive = path.clone();
        path.clone()
    };
    let c = crate::support::backup::verify(&path)?;
    let here = crate::support::backup::contents(&conn)?;

    if !yes {
        // A restore replaces the store. Asking is the difference between recovering from a backup
        // and destroying a repository because the wrong file was named.
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Usage,
            format!(
                "this replaces {} change(s) here with {} from {}. Pass --yes to do it",
                here.changesets,
                c.changesets,
                named.display()
            ),
        )));
    }

    drop(conn);
    let restored = crate::support::backup::restore(&root, &path)?;
    if sealed {
        // The unsealed copy is the whole history in the clear; it does not outlive the command.
        let _ = std::fs::remove_file(&_keep_alive);
    }
    println!(
        "restored {} change(s), {} object(s), {} branch(es), {} tag(s) from {}",
        restored.changesets,
        restored.objects,
        restored.branches,
        restored.tags,
        named.display()
    );
    Ok(())
}

pub fn cmd_restore(file: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // No argument stays an error here, unlike `checkout`: this command throws away work that was
    // never committed, and a default that discards the whole tree would fire on a typo. The
    // refusal names the form instead of leaving the reader to find it in `--help`.
    let file = file.ok_or_else(|| {
        bad("restore needs a file, or `.` for the whole working tree: `gesta restore .` discards every uncommitted change".to_string())
    })?;
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

    // stale pending check - same as text output
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?;
    let stale = if !pending.is_empty() {
        pending_out_of_date(&conn, &root, total)?
    } else {
        Vec::new()
    };
    let stale_json: Vec<Json> = stale
        .into_iter()
        .map(|(id, paths)| {
            Json::obj(vec![
                ("id", Json::str(id)),
                (
                    "paths",
                    Json::arr(paths.into_iter().map(Json::str).collect()),
                ),
            ])
        })
        .collect();

    emit(Json::obj(vec![
        ("branch", Json::str(branch)),
        ("head", head.map(Json::str).unwrap_or(Json::Null)),
        ("pending_changesets", Json::arr(pending)),
        ("stale_pending", Json::arr(stale_json)),
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

pub fn cmd_diff_json(changeset: Option<&str>, name_only: bool) -> YResultAlias {
    let (root, conn) = open_repo()?;
    match changeset {
        Some(id) => {
            let files = repo::changeset_diff(&conn, id)?;
            let renames = history::detect_renames(&conn, id, 0.5)?;
            // `--name-only` asked for a list of paths and nothing else. Accepting the flag and
            // printing the full per-file bodies anyway answers with more than was asked, and a
            // caller that piped the result into `wc -l` gets the wrong count, not a longer one.
            if name_only {
                emit(Json::obj(vec![
                    ("changeset", Json::str(id)),
                    (
                        "files",
                        Json::arr(files.into_iter().map(|(p, _, _)| Json::str(p)).collect()),
                    ),
                ]));
                return Ok(());
            }
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
pub fn cmd_diff_range_json(from: Option<&str>, to: Option<&str>, name_only: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let from = resolve_from(&conn, from, to)?;
    let diffs = crate::repo::diff_revs(&conn, from.as_deref(), to)?;
    if diffs.is_empty() {
        emit(Json::obj(vec![
            ("from", from.map(Json::str).unwrap_or(Json::Null)),
            ("to", Json::str(to.unwrap_or("an empty tree"))),
            ("files", Json::arr(Vec::new())),
        ]));
        return Ok(());
    }
    use crate::support::json::Json;
    let files: Vec<Json> = diffs
        .into_iter()
        .map(|(path, removed, added)| {
            if name_only {
                return Json::str(path);
            }
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
        .collect();
    emit(Json::obj(vec![
        ("from", from.map(Json::str).unwrap_or(Json::Null)),
        ("to", Json::str(to.unwrap_or("an empty tree"))),
        ("files", Json::arr(files)),
    ]));
    Ok(())
}
