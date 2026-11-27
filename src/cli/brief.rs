//! One call that answers where this session stands.
//!
//! An agent starting work has to know what it inherited: which branch, whether a rebase or
//! merge stopped half way, what is stashed, what is uncommitted, what is pending. Asking that
//! with four commands costs four round trips and four blocks of output to hold in context. This
//! puts the answer in one small object, and every field is something a decision depends on.

use crate::support::json::Json;
use crate::ZResult;
use rusqlite::Connection;
use std::path::Path;

/// What to do next, stated as one of a few known states.
///
/// A free text summary would be read once and forgotten. A named state can be branched on by a
/// program, which is the difference between an agent that checks and an agent that guesses.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Next {
    /// A rebase stopped on a conflict. It must be resolved or aborted before anything else.
    FinishRebase,
    /// Tracked files differ from the head. Commit them or they are lost on the next switch.
    CommitWork,
    /// A changeset was snapshotted but never committed, and it still matches the disk.
    CommitPending,
    /// Nothing to do: the tree matches the head and no operation is half finished.
    Ready,
}

impl Next {
    pub fn as_str(self) -> &'static str {
        match self {
            Next::FinishRebase => "finish-rebase",
            Next::CommitWork => "commit-work",
            Next::CommitPending => "commit-pending",
            Next::Ready => "ready",
        }
    }

    /// What an agent should do, in the words it needs. Empty when nothing is owed.
    pub fn advice(self) -> &'static str {
        match self {
            Next::FinishRebase => {
                "a rebase stopped on a conflict: resolve the files, then `gesta rebase --continue`, or `gesta rebase --abort` to put the branch back"
            }
            Next::CommitWork => {
                "the working tree differs from the head: `gesta changeset create` then `gesta changeset commit`, or `gesta stash save` to set it aside"
            }
            Next::CommitPending => {
                "a pending changeset describes the disk: `gesta changeset commit <id>`, or discard it"
            }
            Next::Ready => "",
        }
    }
}

/// Everything a session needs to know before its first edit.
pub struct Brief {
    pub branch: String,
    pub head: Option<String>,
    pub next: Next,
    pub pending: Vec<String>,
    /// Changesets a stopped rebase still has to replay.
    pub rebase_pending: usize,
    pub stashes: usize,
    pub conflicts: Vec<String>,
    /// Files that differ from the head, by kind.
    pub changed: usize,
    /// Stale pending snapshots, which a commit would record wrongly.
    pub stale: Vec<String>,
}

pub fn brief(conn: &Connection, root: &Path) -> ZResult<Brief> {
    let branch = crate::store::current_branch(conn)?;
    let head = crate::store::branch_head(conn, &branch)?;

    let pending: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM changesets WHERE status = 'pending' ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let rebase_pending = crate::repo::rebase::load(conn)?
        .map(|s| s.pending.len())
        .unwrap_or(0);
    let stashes = crate::repo::stash::list(conn)?.len();

    let delta = crate::repo::diff_worktree(conn, root, &branch)?;
    // `absent` is deliberately not counted: nothing is owed for a file that was never here. Counting
    // it made `brief` say the tree "differs from the head" and advise `changeset create` + `commit`,
    // which would record a deletion of content the user never had (#342).
    let changed = delta.added.len() + delta.modified.len() + delta.deleted.len();
    // A `.theirs` file left beside a path is what a stopped merge writes, so its presence is
    // the honest signal for an unresolved conflict. The delta does not carry one.
    let conflicts: Vec<String> = delta
        .modified
        .iter()
        .chain(delta.added.iter())
        .filter(|p| root.join(format!("{}.theirs", p)).exists())
        .cloned()
        .collect();

    // A stale snapshot is one whose recorded content no longer matches the disk, which is what
    // a lost update looks like from the outside.
    let mut stale = Vec::new();
    // Paths a live snapshot already describes. A snapshot that covers the whole working diff is
    // the thing to commit, not something to snapshot again over the top of.
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in &pending {
        let mut stmt = conn.prepare(
            "SELECT cf.path, cf.new_hash FROM changeset_files cf WHERE cf.changeset_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut is_stale = false;
        for row in rows {
            let (path, recorded) = row?;
            // Raw bytes. The scan hashes binary files with `hash_bytes`, and recomputing here as
            // `hash_content(lossy(bytes))` gave a different value whenever the file was not valid
            // UTF-8, so a pending snapshot holding an image was always reported stale and never
            // covered. `brief --fix` then dropped a snapshot that exactly described the disk.
            let on_disk = std::fs::read(root.join(&path))
                .ok()
                .map(|b| crate::store::hash_bytes(&b));
            if recorded != on_disk {
                is_stale = true;
            } else {
                covered.insert(path);
            }
        }
        if is_stale {
            stale.push(id.clone());
        }
    }

    // Work the live snapshots do not describe: a new path, or a file that moved after its
    // snapshot. Only when this is empty does committing the snapshot finish the work.
    let uncovered = delta
        .added
        .iter()
        .chain(delta.modified.iter())
        .chain(delta.deleted.iter())
        .filter(|p| !covered.contains(*p))
        .count();

    let live_pending = pending.iter().any(|id| !stale.contains(id));
    let next = if rebase_pending > 0 {
        Next::FinishRebase
    } else if live_pending && uncovered == 0 {
        Next::CommitPending
    } else if changed > 0 {
        Next::CommitWork
    } else if live_pending {
        Next::CommitPending
    } else {
        Next::Ready
    };

    Ok(Brief {
        branch,
        head,
        next,
        pending,
        rebase_pending,
        stashes,
        conflicts,
        changed,
        stale,
    })
}

pub fn to_json(b: &Brief) -> Json {
    Json::obj(vec![
        ("branch", Json::str(b.branch.clone())),
        (
            "head",
            match &b.head {
                Some(h) => Json::str(h.clone()),
                None => Json::Null,
            },
        ),
        ("next", Json::str(b.next.as_str())),
        ("advice", Json::str(b.next.advice())),
        (
            "pending",
            Json::arr(b.pending.iter().map(|s| Json::str(s.clone())).collect()),
        ),
        ("rebase_pending", Json::int(b.rebase_pending as i64)),
        ("stashes", Json::int(b.stashes as i64)),
        (
            "conflicts",
            Json::arr(b.conflicts.iter().map(|s| Json::str(s.clone())).collect()),
        ),
        ("changed", Json::int(b.changed as i64)),
        (
            "stale",
            Json::arr(b.stale.iter().map(|s| Json::str(s.clone())).collect()),
        ),
    ])
}

/// What a fix did, so the caller reports it rather than guessing.
#[derive(Debug, Default, PartialEq)]
pub struct Fixed {
    /// Pending changesets committed because they described the disk exactly.
    pub committed: Vec<String>,
    /// Pending changesets dropped because the disk had moved past them.
    pub dropped: Vec<String>,
    /// Left alone, with the reason. A fix that silently skips work is worse than no fix.
    pub skipped: Vec<(String, String)>,
    /// A stash made from uncommitted work: its id and how many files it holds.
    pub stashed: Vec<(i64, usize)>,
}

/// What a fix is allowed to do beyond the two actions that need no judgement.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// Set the uncommitted work aside as a stash instead of refusing it.
    ///
    /// Off by default. Committing a snapshot is safe because the snapshot already describes the
    /// disk; a stash is safe because it keeps the work. What is not safe is deciding on someone's
    /// behalf that their unfinished edit should disappear from the tree, so it takes a flag.
    pub stash_uncommitted: bool,
}

/// Do the safe part of what `brief` reports, and name what was left.
///
/// `Options::default()` is the state that needs no judgement; every extra action is opt in.
pub fn fix_with(conn: &Connection, root: &Path, opts: Options) -> ZResult<Fixed> {
    let b = brief(conn, root)?;
    let mut out = Fixed::default();

    if b.next == Next::FinishRebase {
        out.skipped.push((
            "rebase".to_string(),
            "a rebase stopped on a conflict: resolve it, then `gesta rebase --continue`"
                .to_string(),
        ));
        return Ok(out);
    }
    if !b.conflicts.is_empty() {
        out.skipped.push((
            b.conflicts.join(", "),
            "an unresolved conflict is waiting: resolve it by hand".to_string(),
        ));
        return Ok(out);
    }

    for id in &b.pending {
        if b.stale.contains(id) {
            // The snapshot describes content the disk no longer has. Committing it would put
            // back what someone has already changed, so it goes.
            match drop_pending(conn, id) {
                Ok(()) => out.dropped.push(id.clone()),
                Err(e) => out.skipped.push((id.clone(), e.to_string())),
            }
        } else {
            // `--fix` commits on the user's behalf, so it passes the same gate a hand-typed commit
            // does. It used to call `commit_one` directly and could commit past a secret the scanner
            // would have refused.
            match crate::cli::changeset::commit_checked(conn, root, id) {
                Ok(_) => out.committed.push(id.clone()),
                Err(e) => out.skipped.push((id.clone(), e.to_string())),
            }
        }
    }

    // The tree is read again, because committing or dropping a snapshot does not touch the disk:
    // the files still differ from the new head, and the count from before is now wrong.
    let changed = brief(conn, root)?.changed;
    if changed == 0 {
        return Ok(out);
    }
    if opts.stash_uncommitted {
        match crate::repo::stash::save(conn, root, "set aside by brief --fix", "gesta brief --fix")
        {
            Ok((id, files)) => out.stashed.push((id, files)),
            Err(e) => out.skipped.push((
                format!("{} file(s)", changed),
                format!("could not stash: {}", e),
            )),
        }
        // A stash that claimed to work but left the tree dirty would be worse than a refusal.
        let left = brief(conn, root)?.changed;
        if left > 0 {
            out.skipped.push((
                format!("{} file(s)", left),
                "the tree still differs after stashing: check `gesta status`".to_string(),
            ));
        }
        return Ok(out);
    }
    out.skipped.push((
        format!("{} file(s)", changed),
        "the working tree differs from the head and no snapshot describes it: snapshot it with `gesta changeset create`, or set it aside with `gesta stash save`".to_string(),
    ));
    Ok(out)
}

fn drop_pending(conn: &Connection, id: &str) -> ZResult<()> {
    conn.execute(
        "DELETE FROM changeset_files WHERE changeset_id = ?1",
        rusqlite::params![id],
    )?;
    conn.execute(
        "DELETE FROM changesets WHERE id = ?1 AND status = 'pending'",
        rusqlite::params![id],
    )?;
    Ok(())
}

pub fn fixed_to_json(f: &Fixed) -> Json {
    let pair = |v: &Vec<(String, String)>| {
        Json::arr(
            v.iter()
                .map(|(a, b)| {
                    Json::obj(vec![
                        ("what", Json::str(a.clone())),
                        ("why", Json::str(b.clone())),
                    ])
                })
                .collect(),
        )
    };
    Json::obj(vec![
        (
            "committed",
            Json::arr(f.committed.iter().map(|s| Json::str(s.clone())).collect()),
        ),
        (
            "dropped",
            Json::arr(f.dropped.iter().map(|s| Json::str(s.clone())).collect()),
        ),
        ("skipped", pair(&f.skipped)),
        (
            "stashed",
            Json::arr(
                f.stashed
                    .iter()
                    .map(|(id, n)| {
                        Json::obj(vec![
                            ("id", Json::int(*id)),
                            ("files", Json::int(*n as i64)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test gets its own directory. Using the process id made all of them share one, so
    /// tests running in threads clobbered each other's files and four of them failed at once
    /// for a reason that had nothing to do with the code under test.
    fn repo_with_one_commit(name: &str) -> (rusqlite::Connection, std::path::PathBuf) {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-brief-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        crate::repo::tests::commit_files(&conn, "b1", "base", &[("a.txt", "one\n")]);
        crate::store::set_branch_head(&conn, "main", "b1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();
        (conn, dir)
    }

    #[test]
    fn a_clean_tree_is_ready() {
        let (conn, dir) = repo_with_one_commit("clean");
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.next, Next::Ready);
        assert_eq!(b.changed, 0);
        assert!(b.next.advice().is_empty());
        assert_eq!(b.head.as_deref(), Some("b1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_on_disk_asks_for_a_commit() {
        let (conn, dir) = repo_with_one_commit("edited");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.next, Next::CommitWork);
        assert_eq!(b.changed, 1);
        assert!(b.next.advice().contains("changeset create"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_snapshot_that_still_matches_the_disk_asks_to_commit_it() {
        let (conn, dir) = repo_with_one_commit("snapshot");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.pending.len(), 1);
        assert_eq!(
            b.next,
            Next::CommitPending,
            "a matching snapshot is worth committing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_the_disk_has_moved_past_is_marked_stale() {
        let (conn, dir) = repo_with_one_commit("stale");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.stale.len(), 1, "the stale snapshot was not flagged");
        assert_eq!(
            b.next,
            Next::CommitWork,
            "new work outranks a stale snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_rebase_outranks_everything_else() {
        let (conn, dir) = repo_with_one_commit("rebase");
        // The rebase table is made on first use, so touch it before writing a row by hand.
        crate::repo::rebase::load(&conn).unwrap();
        conn.execute(
            "INSERT INTO rebase(id, branch, onto, pending, current, done, original_head, created_at)
             VALUES(1, 'side', 'main', '[\"x\",\"y\"]', 'x', '[]', 'b1', 1)",
            [],
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(b.next, Next::FinishRebase);
        assert_eq!(b.rebase_pending, 2);
        assert!(b.next.advice().contains("rebase --continue"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_snapshot_that_covers_the_diff_is_committed_before_snapshotting_again() {
        let (conn, dir) = repo_with_one_commit("covers");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(
            b.next,
            Next::CommitPending,
            "committing the snapshot is the next step, not making another one"
        );
        assert!(b.next.advice().contains("commit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_work_beyond_a_snapshot_asks_for_a_commit_not_a_lost_edit() {
        let (conn, dir) = repo_with_one_commit("beyond");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        // A file the snapshot never saw.
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let b = brief(&conn, &dir).unwrap();
        assert_eq!(
            b.next,
            Next::CommitWork,
            "the snapshot does not describe b.txt, so committing it would drop the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn fix_commits_a_snapshot_that_matches_the_disk() {
        let (conn, dir) = repo_with_one_commit("fix-commit");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let cs = crate::repo::create_changeset(&conn, &dir, "snap", "a")
            .unwrap()
            .unwrap();
        let f = fix_with(&conn, &dir, Options::default()).unwrap();
        assert_eq!(f.committed, vec![cs.id.clone()]);
        assert!(f.dropped.is_empty());
        assert_eq!(brief(&conn, &dir).unwrap().next, Next::Ready);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_drops_a_snapshot_the_disk_moved_past() {
        let (conn, dir) = repo_with_one_commit("fix-drop");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let cs = crate::repo::create_changeset(&conn, &dir, "snap", "a")
            .unwrap()
            .unwrap();
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();
        let f = fix_with(&conn, &dir, Options::default()).unwrap();
        assert_eq!(f.dropped, vec![cs.id.clone()], "the stale snapshot stayed");
        assert!(
            f.committed.is_empty(),
            "committing it would record stale content"
        );
        // The drops removed the row rather than committing wrong content.
        let still: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM changesets WHERE id = ?1",
                [&cs.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_refuses_to_guess_when_a_rebase_stopped() {
        let (conn, dir) = repo_with_one_commit("fix-rebase");
        crate::repo::rebase::load(&conn).unwrap();
        conn.execute(
            "INSERT INTO rebase(id, branch, onto, pending, current, done, original_head, created_at)
             VALUES(1, 'side', 'main', '[\"x\"]', 'x', '[]', 'b1', 1)",
            [],
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let before = brief(&conn, &dir).unwrap();
        let f = fix_with(&conn, &dir, Options::default()).unwrap();
        assert!(f.committed.is_empty() && f.dropped.is_empty());
        assert!(!f.skipped.is_empty(), "a refusal must say why");
        assert!(f.skipped[0].1.contains("rebase --continue"));
        // Nothing moved.
        assert_eq!(brief(&conn, &dir).unwrap().changed, before.changed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_leaves_uncommitted_work_alone_and_names_the_command() {
        let (conn, dir) = repo_with_one_commit("fix-work");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let f = fix_with(&conn, &dir, Options::default()).unwrap();
        assert!(f.committed.is_empty() && f.dropped.is_empty());
        assert!(f.skipped[0].1.contains("changeset create"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "two\n",
            "the edit was touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn stash_is_not_used_unless_it_is_asked_for() {
        let (conn, dir) = repo_with_one_commit("opt-off");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let f = fix_with(&conn, &dir, Options::default()).unwrap();
        assert!(
            f.stashed.is_empty(),
            "the edit was set aside without being asked"
        );
        assert_eq!(
            brief(&conn, &dir).unwrap().changed,
            1,
            "the edit left the tree"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asking_for_it_sets_the_work_aside_and_leaves_a_clean_tree() {
        let (conn, dir) = repo_with_one_commit("opt-on");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        let f = fix_with(
            &conn,
            &dir,
            Options {
                stash_uncommitted: true,
            },
        )
        .unwrap();
        assert_eq!(f.stashed.len(), 1);
        assert_eq!(
            f.stashed[0].1, 2,
            "both the edit and the new file were saved"
        );
        assert!(
            f.skipped.is_empty(),
            "nothing should be left: {:?}",
            f.skipped
        );
        assert_eq!(
            brief(&conn, &dir).unwrap().changed,
            0,
            "the tree is still dirty"
        );
        // The head state came back, which is what makes the tree clean.
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(!dir.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stashing_after_committing_a_snapshot_still_catches_what_is_left() {
        // Committing changes the head but not the disk, so a count taken before it is wrong.
        // This is the defect the second read of the tree exists to prevent.
        let (conn, dir) = repo_with_one_commit("opt-both");
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        crate::repo::create_changeset(&conn, &dir, "snap", "a").unwrap();
        std::fs::write(dir.join("c.txt"), "later\n").unwrap();
        let f = fix_with(
            &conn,
            &dir,
            Options {
                stash_uncommitted: true,
            },
        )
        .unwrap();
        assert_eq!(f.committed.len(), 1, "the snapshot was not committed");
        assert_eq!(
            f.stashed.len(),
            1,
            "the file added after the snapshot was not stashed"
        );
        assert_eq!(brief(&conn, &dir).unwrap().changed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_rebase_is_refused_even_when_stashing_was_asked_for() {
        let (conn, dir) = repo_with_one_commit("opt-rebase");
        crate::repo::rebase::load(&conn).unwrap();
        conn.execute(
            "INSERT INTO rebase(id, branch, onto, pending, current, done, original_head, created_at)
             VALUES(1, 'side', 'main', '[\"x\"]', 'x', '[]', 'b1', 1)",
            [],
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let f = fix_with(
            &conn,
            &dir,
            Options {
                stash_uncommitted: true,
            },
        )
        .unwrap();
        assert!(
            f.stashed.is_empty(),
            "stashing mid rebase would hide a conflict"
        );
        assert_eq!(brief(&conn, &dir).unwrap().changed, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_binary_file_in_a_pending_snapshot_is_not_stale() {
        // #181: the scan hashes binary files over the raw bytes, and this recomputed the hash as
        // `hash_content(lossy(bytes))`. For a file that is not valid UTF-8 the two differ, so a
        // pending snapshot holding an image was always stale and never covered, and `brief --fix`
        // dropped a snapshot that exactly described the disk.
        let d = std::env::temp_dir().join(format!("zf-brief-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::tests::mem_db();

        let payload: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0xFF, 0xFE, 0x80];
        std::fs::write(d.join("logo.bin"), &payload).unwrap();
        let cs = crate::repo::create_changeset(&conn, &d, "img", "t")
            .unwrap()
            .expect("a new binary file is a changeset");

        let b = brief(&conn, &d).unwrap();
        assert!(
            b.pending.contains(&cs.id),
            "the snapshot should be pending, got {:?}",
            b.pending
        );
        assert!(
            !b.stale.contains(&cs.id),
            "a snapshot that matches the disk must not be stale: {:?}",
            b.stale
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
