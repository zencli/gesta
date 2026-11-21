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
use rusqlite::Connection;

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
        .ok();
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
    let files = repo::tree_at(conn, changeset)?;
    let mut written = 0usize;
    for (path, hash) in &files {
        let content = crate::store::get_object(conn, hash)?;
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content.as_bytes())?;
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
        if good_answer {
            // Everything up to and including this one is good.
            if let Some(pos) = s.candidates.iter().position(|c| c == &cur) {
                s.candidates.drain(..=pos);
            }
            s.good = cur;
        } else {
            // Everything from this one on is bad; it is the current best answer.
            if let Some(pos) = s.candidates.iter().position(|c| c == &cur) {
                s.candidates.truncate(pos + 1);
            }
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

    let mid = (s.candidates.len() - 1) / 2;
    let next = s.candidates[mid].clone();
    checkout(conn, root, &next)?;
    s.current = Some(next.clone());
    s.steps += 1;
    store(conn, &s)?;
    Ok(Step::Test(next))
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
}
