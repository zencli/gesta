//! A path that disappears from a tree while other files still name it.
//!
//! A rebase replays changesets one at a time. When one of them deletes a file and another, made in
//! parallel, adds a line that mentions the deleted name, both changes apply cleanly: the deletion
//! touches the file and the edit touches a different one, so no three-way merge sees a conflict.
//! The rebase reports success and the tree now holds a reference to a path that is not there.
//!
//! Git does exactly this, and the project that named the problem solves it by telling agents to
//! grep for the deleted name afterwards. An index over the whole tree answers that question
//! directly, so this module does the grep and the rebase says what it found.
//!
//! The matching is deliberately narrow. A name is reported only when it appears in another file as
//! a run delimited by non-name characters, which is what a reference looks like: `util.sh` in
//! `source util.sh`, not `util.sh.bak` and not `myutil.sh`. A wider test would report prose and
//! substring matches, and a warning that fires on those is a warning people learn to skip.

use crate::ZResult;
use rusqlite::Connection;

/// A file that gone from the tree, and one file that still names it.
#[derive(Debug, Clone, PartialEq)]
pub struct Dangling {
    /// The path that is no longer in the tree.
    pub removed: String,
    /// A file still in the tree whose text contains that name.
    pub used_by: String,
    /// The line number in `used_by`, counted from 1.
    pub line: usize,
    /// The line itself, trimmed, so the reader does not have to open the file to judge it.
    pub text: String,
}

/// Names removed between two trees, with the files that still mention them, and the files whose
/// bodies could not be read so the search never covered them.
///
/// `before` is the tree the branch had, `after` is the tree the branch has now. A path present in
/// `after` is never reported, however it got there: the question is only about what left.
///
/// Skipping an unreadable body silently is the one thing this check must not do: the question is
/// whether the tree holds a reference to a path that has gone, and an object that cannot be read is
/// exactly the damage that hides the answer. The rebase did succeed, so this is not an error - but
/// the reader has to know the search was incomplete.
///
/// The `skipped` list holds paths, not errors: what the reader can act on is which files went
/// unchecked, and the store error behind each one would bury that in repetition.
pub fn find_with_skips(
    conn: &Connection,
    before: &str,
    after: &str,
) -> ZResult<(Vec<Dangling>, Vec<String>)> {
    let old = crate::store::tree_at(conn, before)?;
    let new: std::collections::HashMap<String, String> =
        crate::store::tree_at(conn, after)?.into_iter().collect();

    let removed: Vec<&(String, String)> =
        old.iter().filter(|(p, _)| !new.contains_key(p)).collect();
    if removed.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut out: Vec<Dangling> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    // Sorted so two runs over the same pair of trees produce the same report; a caller diffing two
    // reports should not have to sort first.
    let mut live: Vec<(&String, &String)> = new.iter().collect();
    live.sort_by(|a, b| a.0.cmp(b.0));

    for (path, hash) in &live {
        // A binary body has no lines to quote and no references to find. Reading it as text would
        // also mean inventing a decoding, and the report is not worth a guess. That is a decision
        // about the content, so it is not a skip and is not reported as one.
        //
        // A body that cannot be read at all is different: nothing was decided, so the file goes in
        // `skipped` rather than vanishing from a check whose purpose is catching quiet damage.
        let bytes = match crate::store::get_object_bytes(conn, hash) {
            Ok(b) => b,
            Err(_) => {
                skipped.push((*path).clone());
                continue;
            }
        };
        if crate::support::attributes::is_binary(&bytes) {
            continue;
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (i, line) in text.lines().enumerate() {
            for (gone, _) in &removed {
                // A path is a reference only when its whole name is there. Counting the base name
                // alone would report `util.sh` for a line that says `myutil.sh`, and the reader
                // would open the file to find nothing wrong.
                if mentions(line, gone) {
                    out.push(Dangling {
                        removed: (*gone).clone(),
                        used_by: (*path).clone(),
                        line: i + 1,
                        text: line.trim().to_string(),
                    });
                }
            }
        }
    }
    // One entry per (removed, used_by) pair is enough to act on; every line in a file that names
    // the same path would bury the report in its own repetition. The first line is kept because it
    // is the one the reader will look at.
    out.sort_by(|a, b| {
        a.removed
            .cmp(&b.removed)
            .then(a.used_by.cmp(&b.used_by))
            .then(a.line.cmp(&b.line))
    });
    out.dedup_by(|a, b| a.removed == b.removed && a.used_by == b.used_by);
    Ok((out, skipped))
}

/// Whether `line` contains `name` as a whole reference rather than as part of a longer name.
///
/// Both ends matter and they test different things. Before the name, a character that can be part
/// of a path means the name is a suffix of something longer (`myutil.sh`). After it, the same
/// applies (`util.sh.bak`, `util.shlib`).
pub fn mentions(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(at) = line[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before_ok = start == 0
            || !line[..start]
                .chars()
                .next_back()
                .map(is_name_char)
                .unwrap_or(false);
        let after_ok = end == line.len()
            || !line[end..]
                .chars()
                .next()
                .map(is_name_char)
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        // Advance past this occurrence, one char not one byte, so a multibyte name cannot split.
        from = start
            + line[start..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
        if from >= line.len() {
            break;
        }
    }
    false
}

/// Characters that continue a path or a name, so a match touching one is a longer name.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/' || c == '\\'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_whose_body_cannot_be_read_is_named_rather_than_skipped() {
        // The loop used to `continue` on an unreadable object, so a dangling reference inside that
        // file was never found and the report looked clean. That is the one result this check must
        // not produce: unreadable storage is exactly the damage that hides the answer.
        let conn = crate::store::tests::mem_db();
        crate::repo::tests::commit_files(
            &conn,
            "c1",
            "base",
            &[
                ("util.sh", "helper\n"),
                ("main.sh", "source util.sh\n"),
                ("notes.md", "see util.sh\n"),
            ],
        );
        let before = crate::store::branch_head(&conn, "main").unwrap().unwrap();

        // Remove one body and leave its tree row: a store that lost a body looks like this.
        let gone: String = conn
            .query_row(
                "SELECT new_hash FROM changeset_files WHERE changeset_id = 'c1' AND path = 'notes.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "DELETE FROM objects WHERE hash = ?1",
            rusqlite::params![gone],
        )
        .unwrap();

        // A second commit deletes util.sh, so something has left the tree and the search runs.
        crate::repo::tests::commit_files(
            &conn,
            "c2",
            "drop util",
            &[("main.sh", "source util.sh\n")],
        );
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES ('c2', 'util.sh', (SELECT new_hash FROM changeset_files
                                       WHERE changeset_id = 'c1' AND path = 'util.sh'), NULL)",
            [],
        )
        .unwrap();
        let after = crate::store::branch_head(&conn, "main").unwrap().unwrap();

        let (found, skipped) = find_with_skips(&conn, &before, &after).unwrap();
        // main.sh is readable and still names the path, so the reference is found.
        assert!(
            found
                .iter()
                .any(|d| d.used_by == "main.sh" && d.removed == "util.sh"),
            "the readable reference must still be reported, got {found:?}"
        );
        // notes.md cannot be read, so it is named as unchecked instead of disappearing.
        assert_eq!(
            skipped,
            vec!["notes.md".to_string()],
            "an unreadable body must be named, got {skipped:?}"
        );
    }
    #[test]
    fn a_name_on_its_own_is_a_reference() {
        assert!(mentions("source util.sh", "util.sh"));
        assert!(mentions("import util.sh;", "util.sh"));
        assert!(mentions("see `util.sh`", "util.sh"));
    }

    #[test]
    fn a_name_inside_a_longer_name_is_not() {
        assert!(!mentions("source myutil.sh", "util.sh"));
        assert!(!mentions("cp a util.sh.bak", "util.sh"));
        assert!(!mentions("run util.shlib", "util.sh"));
        assert!(!mentions("./util.shx", "util.sh"));
    }

    #[test]
    fn a_path_used_as_a_path_is_a_reference() {
        // The whole path is a reference to the whole path. A path ending in the same base name is
        // a different file, so `lib/util.sh` does not mention `util.sh` and must not be reported
        // for it: deleting `util.sh` leaves `lib/util.sh` perfectly valid.
        assert!(!mentions("call lib/util.sh now", "util.sh"));
        assert!(mentions("= lib/util.sh", "lib/util.sh"));
        assert!(!mentions("= lib/util.sh.bak", "lib/util.sh"));
        assert!(!mentions("source ./util.sh", "util.sh"));
    }

    #[test]
    fn a_multibyte_name_does_not_split() {
        // The loop advances by one char, so a name that is not ASCII cannot land mid-codepoint.
        assert!(mentions("x tệp.txt y", "tệp.txt"));
        assert!(!mentions("x mytệp.txt y", "tệp.txt"));
    }

    #[test]
    fn an_empty_name_matches_nothing() {
        assert!(!mentions("anything", ""));
    }
}
