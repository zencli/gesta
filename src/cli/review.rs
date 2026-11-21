//! The review commands.

use super::*;

/// Do the safe half of what `brief` reports.
pub fn cmd_brief_fix(json_out: bool, stash: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let f = crate::cli::brief::fix_with(
        &conn,
        &root,
        crate::cli::brief::Options {
            stash_uncommitted: stash,
        },
    )?;
    if json_out {
        emit(crate::cli::brief::fixed_to_json(&f));
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
    let b = crate::cli::brief::brief(&conn, &root)?;
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
    let b = crate::cli::brief::brief(&conn, &root)?;
    emit(crate::cli::brief::to_json(&b));
    Ok(())
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
    let rows: Vec<crate::support::json::Json> = hits
        .iter()
        .map(|h| {
            crate::support::json::Json::obj(vec![
                ("path", crate::support::json::Json::str(h.path.clone())),
                ("line", crate::support::json::Json::int(h.line as i64)),
                ("rev", crate::support::json::Json::str(h.rev.clone())),
                ("text", crate::support::json::Json::str(h.text.clone())),
            ])
        })
        .collect();
    emit(crate::support::json::Json::obj(vec![
        (
            "pattern",
            crate::support::json::Json::str(s.pattern.clone()),
        ),
        ("history", crate::support::json::Json::Bool(s.history)),
        ("count", crate::support::json::Json::int(hits.len() as i64)),
        ("hits", crate::support::json::Json::arr(rows)),
    ]));
    Ok(())
}

/// Build the search from the flags, refusing `--regex` in a build that does not have it.
fn build_pattern(
    needle: &str,
    ignore_case: bool,
    word: bool,
    regex: bool,
) -> ZResult<crate::support::grep::Pattern> {
    #[cfg(feature = "regex")]
    if regex {
        return crate::support::grep::Pattern::regex(needle, ignore_case, word);
    }
    #[cfg(not(feature = "regex"))]
    if regex {
        return Err(bad(
            "this build has no regular expression support: rebuild with --features regex, or search for literal text",
        ));
    }
    let _ = regex;
    crate::support::grep::Pattern::new(needle, ignore_case, word)
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

fn grep_hits(s: &Search) -> ZResult<Vec<crate::support::grep::Hit>> {
    let (root, conn) = open_repo()?;
    let pattern = build_pattern(&s.pattern, s.ignore_case, s.word, s.regex)?;
    let filter = s.path.as_deref().map(normalise);
    if s.history {
        crate::support::grep::search_history(&conn, &pattern, filter.as_deref(), s.limit)
    } else {
        crate::support::grep::search_worktree(&conn, &root, &pattern, filter.as_deref())
    }
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
                crate::support::json::Json::obj(vec![
                    ("path", crate::support::json::Json::str(path.clone())),
                    (
                        "removed",
                        crate::support::json::Json::arr(
                            removed
                                .iter()
                                .map(|l| crate::support::json::Json::str(l.clone()))
                                .collect(),
                        ),
                    ),
                    (
                        "added",
                        crate::support::json::Json::arr(
                            added
                                .iter()
                                .map(|l| crate::support::json::Json::str(l.clone()))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();

        let stale = crate::support::impact::stale_references(&conn, &step.files, &tree)
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                crate::support::json::Json::obj(vec![
                    ("name", crate::support::json::Json::str(s.name)),
                    ("path", crate::support::json::Json::str(s.path)),
                    ("line", crate::support::json::Json::int(s.line as i64)),
                    ("text", crate::support::json::Json::str(s.text)),
                    (
                        "removed_from",
                        crate::support::json::Json::str(s.removed_from),
                    ),
                ])
            })
            .collect();

        items.push(crate::support::json::Json::obj(vec![
            (
                "changeset",
                crate::support::json::Json::str(step.id.clone()),
            ),
            (
                "message",
                crate::support::json::Json::str(step.message.clone()),
            ),
            (
                "author",
                crate::support::json::Json::str(step.author.clone()),
            ),
            ("risk", crate::support::json::Json::str(step.risk.band())),
            ("score", crate::support::json::Json::int(step.risk.score)),
            (
                "reasons",
                crate::support::json::Json::arr(
                    step.risk
                        .reasons
                        .iter()
                        .map(|r| crate::support::json::Json::str(r.clone()))
                        .collect(),
                ),
            ),
            ("files", crate::support::json::Json::arr(files)),
            ("stale", crate::support::json::Json::arr(stale)),
        ]));
    }

    emit_object(vec![
        (
            "branch",
            crate::support::json::Json::str(target.to_string()),
        ),
        (
            "from",
            match from {
                Some(f) => crate::support::json::Json::str(f.to_string()),
                None => crate::support::json::Json::Null,
            },
        ),
        ("shown", crate::support::json::Json::int(items.len() as i64)),
        ("below_threshold", crate::support::json::Json::int(hidden)),
        ("steps", crate::support::json::Json::arr(items)),
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
            match crate::support::impact::stale_references(&conn, &step.files, &tree) {
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
                    let blind = crate::support::impact::blind_spots(&conn, &tree);
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
