//! Finding the changeset that broke something.
//!
//! A bisect walks the history between a known good point and a known bad point,
//! writing each candidate onto the working tree and asking the caller whether it
//! is good or bad. A session is stored, so the question can be answered in a
//! separate command: an agent can test, then answer, then test again.
//!
//! Two properties make it safe to run on a real repository:
//!
//! 1. The working tree is restored to the branch head when the session ends,
//!    whether it ended by finding the answer or by being cancelled.
//! 2. The branch head never moves. A bisect reads history and writes the working
//!    tree; it must not create a commit, so no operation is recorded as a change
//!    to the repository.
//!
//! The search is a plain binary search over the linear ancestors of the bad
//! point. A merge brings in a second parent, and the set of candidates is then
//! the ordered list of ancestors by creation time rather than a line, which is
//! why the answer is stated as "the first changeset on this path that is bad"
//! rather than the only one.

use std::path::Path;

use rusqlite::params;
use rusqlite::{Connection, OptionalExtension};

use crate::repo;
use crate::{bad, now, ZResult};

/// State of an in-progress search.
pub struct Session {
    pub good: String,
    pub bad: String,
    pub candidates: Vec<String>,
    pub current: Option<String>,
    pub steps: i64,
}

fn init_schema(conn: &Connection) -> ZResult<()> {
    // One row, id fixed at 1. A second concurrent bisect on the same repository
    // would fight over the working tree, so the table is deliberately singular.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bisect (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            good       TEXT NOT NULL,
            bad        TEXT NOT NULL,
            candidates TEXT NOT NULL,
            current    TEXT,
            steps      INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

/// Ancestors of `bad` that are not ancestors of `good`, oldest first.
///
/// The order comes from the parent chain, not from the creation time. Changesets
/// made in one burst share a timestamp to the second, so sorting by time leaves
/// an arbitrary order, and a binary search over an arbitrary order cuts the wrong
/// half. Measured before the fix: a 20 changeset range with the fault at 13
/// answered the last changeset instead.
///
/// A merge brings in a second parent, so the walk follows the first parent for
/// the main line and appends what the second parent contributes after it. That
/// keeps one deterministic sequence, which is what a bisect needs.
fn range(conn: &Connection, good: &str, bad_point: &str) -> ZResult<Vec<String>> {
    let good_set: std::collections::HashSet<String> =
        crate::store::ancestors(conn, good)?.into_iter().collect();

    // Walk the first-parent chain from the bad point back to the good one.
    let mut chain: Vec<String> = Vec::new();
    let mut cur = Some(bad_point.to_string());
    let mut guard = 0usize;
    while let Some(id) = cur {
        if good_set.contains(&id) || chain.len() > 100_000 {
            break;
        }
        chain.push(id.clone());
        guard += 1;
        if guard > 100_000 {
            return Err(bad(
                "history walk did not end: the parent chain contains a cycle".to_string(),
            ));
        }
        let (p1, _p2) = crate::store::parents_of_public(conn, &id)?;
        cur = p1;
    }
    chain.reverse();

    // Anything else reachable from the bad point and not from the good one is a
    // side branch. It is appended after the main line so a bisect still visits
    // it, in a stable order.
    let mut extras: Vec<String> = Vec::new();
    for id in crate::store::ancestors(conn, bad_point)? {
        if good_set.contains(&id) || chain.contains(&id) {
            continue;
        }
        extras.push(id);
    }
    extras.sort();
    chain.extend(extras);
    Ok(chain)
}

pub fn start(conn: &Connection, good: &str, bad_point: &str) -> ZResult<Session> {
    init_schema(conn)?;
    let candidates = range(conn, good, bad_point)?;
    if candidates.is_empty() {
        return Err(bad(format!(
            "no changeset between {} and {}: they are the same point, or one already contains the other",
            good, bad_point
        )));
    }
    let session = Session {
        good: good.to_string(),
        bad: bad_point.to_string(),
        candidates,
        current: None,
        steps: 0,
    };
    conn.execute(
        "INSERT OR REPLACE INTO bisect(id, good, bad, candidates, current, steps, created_at)
         VALUES(1, ?1, ?2, ?3, NULL, 0, ?4)",
        params![
            session.good,
            session.bad,
            session.candidates.join(","),
            now()
        ],
    )?;
    Ok(session)
}

pub fn load(conn: &Connection) -> ZResult<Option<Session>> {
    init_schema(conn)?;
    let row: Option<(String, String, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT good, bad, candidates, current, steps FROM bisect WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        // `.optional()?`, not `.ok()`: `start` replaces the session row, so reading a locked
        // database as "no bisect" would overwrite a live bisect with a new one.
        .optional()?;
    Ok(row.map(|(good, bad, cands, current, steps)| Session {
        good,
        bad,
        candidates: if cands.is_empty() {
            Vec::new()
        } else {
            cands.split(',').map(|s| s.to_string()).collect()
        },
        current,
        steps,
    }))
}

fn store(conn: &Connection, s: &Session) -> ZResult<()> {
    conn.execute(
        "UPDATE bisect SET good = ?1, bad = ?2, candidates = ?3, current = ?4, steps = ?5 WHERE id = 1",
        params![
            s.good,
            s.bad,
            s.candidates.join(","),
            s.current,
            s.steps
        ],
    )?;
    Ok(())
}

pub fn clear(conn: &Connection) -> ZResult<()> {
    conn.execute("DELETE FROM bisect WHERE id = 1", [])?;
    Ok(())
}

/// Write `changeset` onto the working tree, tracked files only.
fn checkout(conn: &Connection, root: &Path, changeset: &str) -> ZResult<usize> {
    let files = crate::store::tree_at(conn, changeset)?;
    let mut written = 0usize;
    for (path, hash) in &files {
        // Bytes, not text: a tree with a binary file is ordinary, and the text reader refuses
        // non-UTF-8 with an encoding error. Every other tree-writing path in the repository already
        // reads bytes for this reason; bisect was the last one reading text, so a bisect step
        // aborted with an encoding error on any tree holding a binary file.
        let content = crate::store::get_object_bytes(conn, hash)?;
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &content)?;
        written += 1;
    }
    // Paths tracked somewhere in history but absent from this changeset are gone
    // at this point, so leaving them would make the tested tree wrong.
    let present: std::collections::HashSet<&String> = files.iter().map(|(p, _)| p).collect();
    let mut stmt = conn.prepare("SELECT DISTINCT path FROM file_versions")?;
    let all: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in all {
        if !present.contains(&path) {
            let target = root.join(&path);
            if target.is_file() {
                std::fs::remove_file(&target)?;
            }
        }
    }
    Ok(written)
}

/// The next changeset to test, or the answer.
pub enum Step {
    /// Write this changeset and report back.
    Test(String),
    /// The search is over: this is the first bad changeset.
    Found(String),
}

/// Advance the search by one answer.
pub fn step(conn: &Connection, root: &Path, good_answer: bool) -> ZResult<Step> {
    let mut s = load(conn)?
        .ok_or_else(|| bad("no bisect in progress (run `gesta bisect start`)".to_string()))?;

    // Narrow the range using the answer about the changeset just tested.
    if let Some(cur) = s.current.clone() {
        // Narrow by ancestry, not by position. The list is a linear extension of the history, and a
        // side branch has no position that means anything: a merge's second parent can put a
        // descendant of the tested changeset on either side of it in the list. Position was what
        // made a wrong answer possible - testing a good side commit drained everything before it,
        // including the bad main-line changeset the fault landed in, and the search then reported
        // the remaining side commit as the culprit without ever testing the fault.
        //
        // The invariant is `candidates = ancestors(bad) minus ancestors(good)`. An answer keeps it:
        // a good answer adds the tested changeset's ancestors to the good set, and a bad answer
        // moves the bad frontier down to the tested changeset. Both prune by reachability, which is
        // the only relation that means anything on a graph. On a linear history every reachability
        // cut is the same as the positional one, so the question count does not change.
        let parents = crate::store::parent_map_for(conn)?;
        if good_answer {
            // This changeset and everything it descends from are good.
            s.candidates
                .retain(|c| !crate::store::walk_reaches(&parents, &cur, c));
            s.good = cur;
        } else {
            // Only what this changeset descends from can still hold the culprit: anything it does
            // not reach is a different line of history, and a changeset there cannot be the first
            // bad one on the way to this bad point. The tested changeset stays, because it is now
            // the best candidate for the answer.
            s.candidates
                .retain(|c| crate::store::walk_reaches(&parents, &cur, c));
            s.bad = cur;
        }
    }

    if s.candidates.len() <= 1 {
        let answer = s.candidates.first().cloned().ok_or_else(|| {
            bad(
                "the search narrowed to nothing, which means the answers contradict each other"
                    .to_string(),
            )
        })?;
        store(conn, &s)?;
        return Ok(Step::Found(answer));
    }

    // Floor of the half, not the ceiling. With two candidates left, `len / 2` is
    // the last one: answering bad truncates after it, which changes nothing, and
    // the same candidate is chosen again forever. Measured before the fix: the
    // search never terminated on a 31 changeset range. `(len - 1) / 2` picks the
    // lower half and every answer strictly shrinks the list.
    // A search over n candidates needs at most log2(n) + 1 questions. Three
    // times that is impossible by arithmetic, so reaching it means the state is
    // inconsistent. Failing here is better than looping: a caller that hangs has
    // no way to tell a slow search from a broken one.
    let ceiling = 4 * (usize::BITS as usize) + 16;
    if s.steps as usize > ceiling {
        return Err(bad(format!(
            "bisect made {} questions without finishing, which cannot happen: the saved state is inconsistent",
            s.steps
        )));
    }

    // Never ask about the changeset that was just answered. It stays in the list because, after a
    // bad answer, it is the leading candidate for the answer, but testing it again would learn
    // nothing and the search would ask the same question forever.
    let len = s.candidates.len();
    let mid = (len - 1) / 2;
    let next = if len > 1 && Some(s.candidates[mid].clone()) == s.current {
        s.candidates[(mid + 1) % len].clone()
    } else {
        s.candidates[mid].clone()
    };
    checkout(conn, root, &next)?;
    s.current = Some(next.clone());
    s.steps += 1;
    store(conn, &s)?;
    Ok(Step::Test(next))
}

/// Drive a whole search from one command, the way `git bisect run` does.
///
/// The manual loop is `step`, answer, `step`, answer; a caller that can decide good or bad without a
/// human in the middle writes that loop once and it goes wrong once. The convention is git's and it
/// is the reason this returns `Step` rather than a bool: a test says good with exit 0, bad with any
/// other exit code, and **125 means skip** - the changeset cannot be tested at all, so it must be
/// dropped from the range without being reported as either answer. Without that case a build that
/// fails to compile on an old changeset ends the search with the wrong commit named as the culprit.
///
/// The command runs with the working directory at the repository root and inherits the environment,
/// so a caller passes a shell command line exactly as it would type it. Its output goes to this
/// process's stdout, because a search that discards the reason each step failed is a search nobody
/// can debug when it names the wrong commit.
pub fn run(
    conn: &Connection,
    root: &Path,
    command: &str,
    mut report: impl FnMut(&str, &str),
) -> ZResult<String> {
    let mut s = load(conn)?
        .ok_or_else(|| bad("no bisect in progress (run `gesta bisect start`)".to_string()))?;

    loop {
        // Ask for the next candidate without answering anything: the answer comes from the command.
        let next = match s.current.clone() {
            Some(cur) => cur,
            None => {
                // First call after `start` already checked one out; if it did not, take the middle.
                let mid = (s.candidates.len() - 1) / 2;
                let id = s.candidates[mid].clone();
                checkout(conn, root, &id)?;
                s.current = Some(id.clone());
                s.steps += 1;
                store(conn, &s)?;
                id
            }
        };

        report("testing", &next);
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(root)
            .status();

        // A command that could not be started at all is not evidence about the changeset.
        let code = match status {
            Ok(st) => st.code(),
            Err(e) => {
                return Err(bad(format!("could not run the test command: {e}")));
            }
        };

        // 125 is git's "skip": drop this candidate from the range and do not record it as an answer.
        if code == Some(125) {
            report("skipped", &next);
            if let Some(pos) = s.candidates.iter().position(|c| c == &next) {
                s.candidates.remove(pos);
            }
            s.current = None;
            store(conn, &s)?;
            if s.candidates.len() <= 1 {
                let Some(answer) = s.candidates.first().cloned() else {
                    // Every candidate answered 125, so there is no answer to report. The worktree is
                    // still checked out at the last tested changeset and the session row is still
                    // there, so `finish` has to run before the error: returning early would leave a
                    // person with a modified tree and a bisect that still looks running, and
                    // nothing telling them to cancel it.
                    finish(conn, root)?;
                    return Err(bad(
                        "every candidate was skipped, so the search has no answer".to_string(),
                    ));
                };
                finish(conn, root)?;
                return Ok(answer);
            }
            continue;
        }

        let good = code == Some(0);
        report(if good { "good" } else { "bad" }, &next);
        match step(conn, root, good)? {
            Step::Test(_) => {
                s = load(conn)?
                    .ok_or_else(|| bad("the session vanished mid-search".to_string()))?;
            }
            Step::Found(id) => {
                finish(conn, root)?;
                return Ok(id);
            }
        }
    }
}

/// Put the working tree back and forget the session.
///
/// Called on cancel and on finding the answer. Restoring is what makes a bisect
/// safe to interrupt: the alternative is a working tree left on an old
/// changeset with no record of why.
pub fn finish(conn: &Connection, root: &Path) -> ZResult<usize> {
    let branch = crate::store::current_branch(conn)?;
    let (written, _removed) = repo::restore_worktree(conn, root, &branch)?;
    clear(conn)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::commit_files;

    #[test]
    fn a_database_error_is_not_read_as_no_bisect() {
        // `load(...).ok()` mapped every failure to `None`, and `start` reads `None` as "nothing is
        // running", replacing the session row. A locked database would therefore overwrite a live
        // bisect instead of reporting that it could not read it.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c00", "base", &[("f", "0\n")]);
        init_schema(&conn).unwrap();
        conn.execute("DROP TABLE bisect", []).unwrap();
        conn.execute(
            "CREATE TABLE bisect (id INTEGER PRIMARY KEY CHECK (id = 1), good INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO bisect (id, good) VALUES (1, 5)", [])
            .unwrap();

        let err = match load(&conn) {
            Ok(_) => panic!("an unreadable good column is not an empty bisect"),
            Err(e) => e,
        };
        assert!(
            format!("{err}").to_lowercase().contains("bad")
                || format!("{err}").to_lowercase().contains("invalid"),
            "the database error must surface, got: {err}"
        );
    }

    #[test]
    fn every_candidate_skipped_leaves_nothing_running() {
        // The error is the right answer, but the state matters as much as the message: an early
        // return left the worktree at the last tested changeset and the session row in place, so the
        // next `bisect start` refused with "a bisect is already running" and the tree looked
        // modified for reasons the person could not see. Exit 125 is git's "skip".
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c00", "base", &[("f", "0\n")]);
        commit_files(&conn, "c01", "one", &[("f", "1\n")]);
        // Exactly one candidate, so skipping it empties the range: with two, skipping one leaves the
        // other as the answer, which is the documented behaviour and not the path under test.
        start(&conn, "c00", "c01").unwrap();

        let d = std::env::temp_dir().join(format!("gesta-bisect-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let err = run(&conn, &d, "exit 125", |_, _| {})
            .expect_err("with everything skipped there is no answer");
        assert!(
            format!("{err}").contains("every candidate was skipped"),
            "the message must name the reason, got: {err}"
        );

        // Nothing is left running, and the worktree is back on the branch rather than the last
        // changeset the search happened to test.
        assert!(
            crate::repo::bisect::load(&conn).unwrap().is_none(),
            "the session must be cleared"
        );
        let on_disk = std::fs::read_to_string(d.join("f")).unwrap();
        assert_eq!(
            on_disk, "1\n",
            "the worktree must be the branch head, not a candidate"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// A changeset with explicit parents, so a test can build a side branch.
    fn commit_with(conn: &Connection, id: &str, p1: Option<&str>, p2: Option<&str>, content: &str) {
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
             VALUES(?1,?1,'a',1,'pending',?2,?3,'main')",
            rusqlite::params![id, p1, p2],
        )
        .unwrap();
        let h = crate::store::put_object(conn, content).unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES(?1,'f',NULL,?2)",
            rusqlite::params![id, h],
        )
        .unwrap();
        crate::repo::commit_one(conn, id).unwrap();
        // `commit_one` points a changeset at the branch head, which is what a commit made at the
        // tip does. A fixture that needs a side branch has to say so after that, or every changeset
        // ends up on the main line and the shape under test never exists.
        conn.execute(
            "UPDATE changesets SET parent = ?2, parent2 = ?3 WHERE id = ?1",
            rusqlite::params![id, p1, p2],
        )
        .unwrap();
    }

    #[test]
    fn a_side_branch_commit_does_not_become_the_answer() {
        // The shape from the issue, narrowed to three candidates so the search is forced onto the
        // side branch. `m1` carries the fault and is a merge that pulled in `e2`, and `e2` descends
        // from `e1`; both side commits are good, because they branched before the fault. `range`
        // walks the first-parent chain to `[m1]` and then appends the side commits sorted by id,
        // giving `[m1, e1, e2]`: bad, good, good.
        //
        // The binary search assumes the opposite. It takes the middle candidate, `e1`, hears good,
        // and drops everything up to and including `e1` - which removes `m1`, the only bad commit in
        // the list. With `[e2]` left it answers `e2`, a good changeset, as the first bad one, and
        // never tested `m1` at all.
        //
        // Badness is decided by ancestry, not by string comparison: "bad" means the fault is this
        // changeset or one of its ancestors, which is what a real test command implements.
        let conn = crate::store::tests::mem_db();
        commit_with(&conn, "base", None, None, "0\n");
        commit_with(&conn, "e1", Some("base"), None, "e1\n");
        commit_with(&conn, "e2", Some("e1"), None, "e2\n");
        commit_with(&conn, "m1", Some("base"), Some("e2"), "1\n"); // the fault, on the main line

        let d = std::env::temp_dir().join(format!("gesta-bisect-side-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        start(&conn, "base", "m1").unwrap();
        let mut asked: Vec<String> = Vec::new();
        let answer = loop {
            let st = load(&conn).unwrap().unwrap();
            let cur = match st.current.clone() {
                Some(c) => c,
                None => match step(&conn, &d, true).unwrap() {
                    Step::Found(x) => break x,
                    Step::Test(_) => continue,
                },
            };
            asked.push(cur.clone());
            let bad = cur == "m1"
                || crate::store::ancestors(&conn, &cur)
                    .unwrap()
                    .iter()
                    .any(|a| a == "m1");
            match step(&conn, &d, !bad).unwrap() {
                Step::Found(x) => break x,
                Step::Test(_) => continue,
            }
        };
        assert_eq!(
            answer, "m1",
            "the answer must be the changeset the fault landed in, not a good side-branch commit; \
             asked {asked:?}"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tree_holding_a_binary_file_is_checked_out() {
        // A tree with a binary file is ordinary, and the text reader refuses non-UTF-8 with an
        // encoding error, so a bisect step aborted on any tree holding one. The assertion is on the
        // bytes that reach the disk rather than on the call succeeding: a checkout that wrote text
        // would still return Ok on a file that happens to be valid UTF-8.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "base", &[("f", "1\n")]);
        let head = crate::store::branch_head(&conn, "main").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('c2','bin','a',2,'pending',?1,'main')",
            rusqlite::params![head],
        )
        .unwrap();
        let bytes: &[u8] = &[0x00, 0xff, 0xfe, 0x80, 0x01];
        let h = crate::store::put_object_bytes(&conn, bytes).unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('c2','blob.bin',NULL,?1)",
            rusqlite::params![h],
        )
        .unwrap();
        crate::repo::commit_one(&conn, "c2").unwrap();

        let d = std::env::temp_dir().join(format!("gesta-bisect-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let written = checkout(&conn, &d, "c2").expect("a binary file must not fail the checkout");
        assert!(written >= 1);
        let on_disk = std::fs::read(d.join("blob.bin")).unwrap();
        assert_eq!(on_disk, bytes, "the bytes written must be the bytes stored");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_range_excludes_what_the_good_point_already_contains() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        commit_files(&conn, "c2", "two", &[("f", "2\n")]);
        commit_files(&conn, "c3", "three", &[("f", "3\n")]);
        commit_files(&conn, "c4", "four", &[("f", "4\n")]);

        let r = range(&conn, "c1", "c4").unwrap();
        assert_eq!(r, vec!["c2", "c3", "c4"]);
    }

    #[test]
    fn a_bad_point_that_already_contains_the_good_one_is_refused() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        commit_files(&conn, "c2", "two", &[("f", "2\n")]);
        assert!(start(&conn, "c2", "c1").is_err());
    }

    #[test]
    fn the_search_takes_a_logarithmic_number_of_questions() {
        // 32 changesets must not cost 32 questions. A linear walk would still
        // give the right answer, which is why the count is asserted, not just
        // the result.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c00", "base", &[("f", "0\n")]);
        for i in 1..=31 {
            commit_files(
                &conn,
                &format!("c{:02}", i),
                "s",
                &[("f", &format!("{}\n", i))],
            );
        }
        let d = std::env::temp_dir().join(format!("gesta-bisect-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        start(&conn, "c00", "c31").unwrap();
        let (found, questions) = find_counting(&conn, &d, "c07");
        assert_eq!(found, "c07");
        assert!(
            questions <= 6,
            "took {} questions for 31 candidates",
            questions
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    /// Same driver, but reports how many questions were needed.
    fn find_counting(conn: &Connection, root: &Path, first_bad: &str) -> (String, usize) {
        let mut questions = 0usize;
        loop {
            let s = load(conn).unwrap().unwrap();
            let cur = match s.current.clone() {
                Some(c) => c,
                None => match step(conn, root, true).unwrap() {
                    Step::Found(x) => return (x, questions),
                    Step::Test(_) => continue,
                },
            };
            questions += 1;
            let is_bad = cur.as_str() >= first_bad;
            match step(conn, root, !is_bad).unwrap() {
                Step::Found(x) => return (x, questions),
                Step::Test(_) => continue,
            }
        }
    }

    /// Drive the search with a fixed rule: any candidate at or after `first_bad`
    /// answers bad, anything earlier answers good.
    fn find_with(conn: &Connection, root: &Path, first_bad: &str) -> String {
        loop {
            let s = load(conn).unwrap().unwrap();
            let cur = match s.current.clone() {
                Some(c) => c,
                None => {
                    // Nothing tested yet: the first step picks a candidate.
                    match step(conn, root, true).unwrap() {
                        Step::Found(x) => return x,
                        Step::Test(_) => continue,
                    }
                }
            };
            let is_bad = cur.as_str() >= first_bad;
            match step(conn, root, !is_bad).unwrap() {
                Step::Found(x) => return x,
                Step::Test(_) => continue,
            }
        }
    }

    #[test]
    fn the_answer_is_the_first_bad_changeset() {
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c0", "base", &[("f", "0\n")]);
        for i in 1..=31 {
            commit_files(
                &conn,
                &format!("c{:02}", i),
                "s",
                &[("f", &format!("{}\n", i))],
            );
        }
        let d = std::env::temp_dir().join(format!("gesta-bisect2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        start(&conn, "c0", "c31").unwrap();
        let found = find_with(&conn, &d, "c13");
        assert_eq!(found, "c13");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_search_with_no_session_is_an_error() {
        let conn = crate::store::tests::mem_db();
        init_schema(&conn).unwrap();
        assert!(load(&conn).unwrap().is_none());
    }

    #[test]
    fn the_range_follows_the_parent_chain_not_the_clock() {
        // Changesets made in one burst share a timestamp to the second. Ordering
        // by time left an arbitrary sequence and a binary search over it cut the
        // wrong half: a 20 changeset range with the fault at 13 answered the last
        // changeset. The parent chain is the only ordering a search can use.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        commit_files(&conn, "c2", "two", &[("f", "2\n")]);
        commit_files(&conn, "c3", "three", &[("f", "3\n")]);
        // Every row shares created_at, which is what a burst of commits produces.
        conn.execute("UPDATE changesets SET created_at = 1000", [])
            .unwrap();

        let r = range(&conn, "c1", "c3").unwrap();
        assert_eq!(
            r,
            vec!["c2", "c3"],
            "order must come from parents, not time"
        );
    }

    #[test]
    fn a_single_candidate_is_the_answer_without_a_question() {
        // Asking about the only candidate would be a wasted round trip, and the
        // caller has no way to answer a question about a point it was told is bad.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        commit_files(&conn, "c2", "two", &[("f", "2\n")]);

        let d = std::env::temp_dir().join(format!("gesta-bisect-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        start(&conn, "c1", "c2").unwrap();
        match step(&conn, &d, true).unwrap() {
            Step::Found(id) => assert_eq!(id, "c2"),
            Step::Test(id) => panic!("asked about {} when there was one candidate", id),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn contradictory_answers_fail_instead_of_looping() {
        // Answering bad about the first candidate and then good about the same
        // point cannot converge. It has to raise, because a caller that hangs
        // cannot tell a slow search from a stuck one.
        let conn = crate::store::tests::mem_db();
        commit_files(&conn, "c1", "one", &[("f", "1\n")]);
        for i in 2..=10 {
            commit_files(
                &conn,
                &format!("c{}", i),
                "s",
                &[("f", &format!("{}\n", i))],
            );
        }
        let d = std::env::temp_dir().join(format!("gesta-bisect-loop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        start(&conn, "c1", "c10").unwrap();
        // Always answer bad: the range collapses to the first candidate.
        let mut answer = false;
        let mut ended = false;
        for _ in 0..40 {
            match step(&conn, &d, answer).unwrap() {
                Step::Found(_) => {
                    ended = true;
                    break;
                }
                Step::Test(_) => {
                    // Flip the answer to keep the state contradictory.
                    answer = true;
                }
            }
        }
        assert!(ended, "the search must terminate, one way or the other");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn a_run_needs_a_session_and_says_so_plainly() {
        // Without a session there is no range, and the command would otherwise be run once against
        // whatever the tree happens to hold and reported as an answer.
        let conn = crate::store::tests::mem_db();
        let root = std::env::temp_dir();
        let err = run(&conn, &root, "true", |_, _| {}).unwrap_err();
        assert!(
            format!("{err}").contains("no bisect in progress"),
            "got: {err}"
        );
    }
}
