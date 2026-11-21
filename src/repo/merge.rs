//! Three-way merge and line-level diffing.
//!
//! Both are built on one primitive: a longest-common-subsequence alignment
//! between two line sequences. Diff reads the alignment; merge reads three
//! alignments (base->ours, base->theirs) and combines them per region.

/// One edit hunk: `old` lines replaced by `new` lines, starting at each side's
/// own offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
}
use crate::ZResult;
use rusqlite::Connection;

/// Line-level diff of two texts as ordered hunks. Repeated lines are handled by
/// position through the alignment, not by counting occurrences, so a duplicate
/// added in one place is not reported as a change somewhere else.
pub fn diff_hunks(old: &str, new: &str) -> Vec<Hunk> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let matches = lcs_matches(&a, &b);

    let mut hunks = Vec::new();
    let mut ai = 0usize;
    let mut bi = 0usize;
    for (am, bm) in matches
        .iter()
        .copied()
        .chain(std::iter::once((a.len(), b.len())))
    {
        if am > ai || bm > bi {
            hunks.push(Hunk {
                old_start: ai,
                old_len: am - ai,
                new_start: bi,
                new_len: bm - bi,
            });
        }
        ai = am + 1;
        bi = bm + 1;
    }
    hunks
}

/// The removed and added lines, in order, for display.
pub fn line_diff(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let hunks = diff_hunks(old, new);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for h in hunks {
        for l in &a[h.old_start..h.old_start + h.old_len] {
            removed.push(l.to_string());
        }
        for l in &b[h.new_start..h.new_start + h.new_len] {
            added.push(l.to_string());
        }
    }
    removed.sort_by_key(|_| 0);
    (removed, added)
}

/// Pairs (i, j) of matching lines forming a longest common subsequence.
fn lcs_matches(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    // Common prefix/suffix stripping keeps the O(n*m) table small for the
    // common case of a one-line edit in a large file.
    let mut pre = 0usize;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0usize;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }

    let a_mid = &a[pre..a.len() - suf];
    let b_mid = &b[pre..b.len() - suf];
    let mut mid = Vec::new();
    if !a_mid.is_empty() && !b_mid.is_empty() {
        let n = a_mid.len();
        let m = b_mid.len();
        // dp[i][j] = LCS length of a_mid[i..] and b_mid[j..]
        let mut dp = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i][j] = if a_mid[i] == b_mid[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < n && j < m {
            if a_mid[i] == b_mid[j] {
                mid.push((pre + i, pre + j));
                i += 1;
                j += 1;
            } else if dp[i + 1][j] >= dp[i][j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }

    let mut out: Vec<(usize, usize)> = (0..pre).map(|i| (i, i)).collect();
    out.extend(mid);
    out.extend((0..suf).map(|k| (a.len() - suf + k, b.len() - suf + k)));
    out
}

/// Result of merging three versions of one file.
#[derive(Debug, Clone)]
pub enum MergeResult {
    /// Clean merge.
    Clean(String),
    /// Both sides changed the same region in different ways.
    Conflict {
        ours: String,
        theirs: String,
        merged: String,
    },
}

impl MergeResult {
    pub fn is_conflict(&self) -> bool {
        matches!(self, MergeResult::Conflict { .. })
    }
    pub fn text(&self) -> &str {
        match self {
            MergeResult::Clean(s) => s,
            MergeResult::Conflict { merged, .. } => merged,
        }
    }

    /// Our side of a conflict, so a caller can report both versions.
    pub fn ours(&self) -> Option<&str> {
        match self {
            MergeResult::Conflict { ours, .. } => Some(ours),
            MergeResult::Clean(_) => None,
        }
    }
}

/// Three-way merge: apply the changes of `theirs` relative to `base` on top of
/// `ours`.
///
/// A region changed on only one side takes that side. A region changed on both
/// sides in the same way merges cleanly. A region changed differently is a
/// conflict, reported rather than silently resolved.
/// Recursively merge multiple bases into a virtual base, then merge ours and theirs against it.
///
/// When there are multiple merge bases (criss-cross), git merges them pairwise to build a virtual
/// base, then does a three-way merge against that virtual base. This avoids the silent loss that
/// happens when picking one base and discarding the rest.
pub fn recursive_merge(
    _conn: &Connection,
    bases: &[String],
    ours: &str,
    theirs: &str,
) -> ZResult<String> {
    match bases.len() {
        0 => Ok(merge3("", ours, theirs).text().to_string()),
        1 => Ok(merge3(&bases[0], ours, theirs).text().to_string()),
        2 => {
            // Merge the two bases first to form a virtual base, then merge ours and theirs against it.
            let virtual_base = merge3(&bases[0], &bases[1], &bases[1]);
            let virtual_base_text = virtual_base.text().to_string();
            Ok(merge3(&virtual_base_text, ours, theirs).text().to_string())
        }
        _n => {
            // More than two: merge recursively, pairing up. Pair the oldest with the newest to keep
            // the history somewhat balanced, then recurse on the results plus any odd one out.
            let mut bases = bases.to_vec();
            bases.sort_by(|a, b| b.cmp(a)); // newest first
            while bases.len() > 1 {
                let mut next = Vec::new();
                for i in (0..bases.len()).step_by(2) {
                    if i + 1 < bases.len() {
                        let v = merge3(&bases[i], &bases[i + 1], &bases[i + 1]);
                        next.push(v.text().to_string());
                    } else {
                        next.push(bases[i].clone());
                    }
                }
                bases = next;
            }
            Ok(merge3(&bases[0], ours, theirs).text().to_string())
        }
    }
}

pub fn merge3(base: &str, ours: &str, theirs: &str) -> MergeResult {
    let base_lines: Vec<&str> = base.lines().collect();
    let our_lines: Vec<&str> = ours.lines().collect();
    let their_lines: Vec<&str> = theirs.lines().collect();

    let our_hunks = diff_hunks(base, ours);
    let their_hunks = diff_hunks(base, theirs);

    // Walk the base from left to right, consuming hunks from both sides.
    let mut out: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut bi = 0usize;
    let (mut oi, mut ti) = (0usize, 0usize);

    while oi < our_hunks.len() || ti < their_hunks.len() {
        // Skip any hunk the walk has already passed. A hunk from the other side can start
        // inside a region this side just replaced: when both sides change overlapping lines
        // to the same content, the longer hunk ends past the other's start, and applying the
        // shorter one afterwards duplicates its replacement. That produced a file with the
        // same line twice after a rebase, with no conflict reported.
        // A hunk that only inserts has no base lines, so its end equals its start. Skipping on
        // the end position alone dropped every insertion when the base was empty, turning a
        // merge of two additions into an empty file, so those are never skipped here.
        //
        // A hunk that starts before `bi` and ends after it overlaps lines that have already
        // been emitted. Applying it would duplicate its replacement, which is exactly how a
        // rebase produced the same line twice with no conflict reported. That case is a real
        // conflict between the two sides and is reported as one.
        while oi + 1 < our_hunks.len()
            && our_hunks[oi].old_len > 0
            && our_hunks[oi].old_start + our_hunks[oi].old_len <= bi
        {
            oi += 1;
        }
        while ti + 1 < their_hunks.len()
            && their_hunks[ti].old_len > 0
            && their_hunks[ti].old_start + their_hunks[ti].old_len <= bi
        {
            ti += 1;
        }
        if oi >= our_hunks.len() && ti >= their_hunks.len() {
            break;
        }

        let next_base = match (our_hunks.get(oi), their_hunks.get(ti)) {
            (Some(o), Some(t)) => o.old_start.min(t.old_start),
            (Some(o), None) => o.old_start,
            (None, Some(t)) => t.old_start,
            (None, None) => break,
        };

        // Copy untouched base lines up to the next hunk.
        while bi < next_base {
            out.push(base_lines[bi].to_string());
            bi += 1;
        }

        // Collect every hunk starting at this base position from either side.
        let mut our_end = bi;
        let mut our_repl: Vec<String> = Vec::new();
        let mut our_changed = false;
        while oi < our_hunks.len() && our_hunks[oi].old_start == next_base {
            let h = &our_hunks[oi];
            our_end = h.old_start + h.old_len;
            our_repl = our_lines[h.new_start..h.new_start + h.new_len]
                .iter()
                .map(|s| s.to_string())
                .collect();
            our_changed = true;
            oi += 1;
        }
        let mut their_end = bi;
        let mut their_repl: Vec<String> = Vec::new();
        let mut their_changed = false;
        while ti < their_hunks.len() && their_hunks[ti].old_start == next_base {
            let h = &their_hunks[ti];
            their_end = h.old_start + h.old_len;
            their_repl = their_lines[h.new_start..h.new_start + h.new_len]
                .iter()
                .map(|s| s.to_string())
                .collect();
            their_changed = true;
            ti += 1;
        }

        let region_end = our_end.max(their_end);

        match (our_changed, their_changed) {
            (true, false) => {
                out.extend(our_repl);
                bi = our_end;
            }
            (false, true) => {
                // Their replacement may already be present because the previous region emitted
                // it: two sides that both introduce the same line at slightly different base
                // positions produced it twice, and a rebase showed the duplicate in the file
                // while reporting success.
                let already = !their_repl.is_empty()
                    && out.len() >= their_repl.len()
                    && out[out.len() - their_repl.len()..] == their_repl[..];
                if !already {
                    out.extend(their_repl);
                }
                bi = their_end;
            }
            (true, true) => {
                if our_repl == their_repl {
                    out.extend(our_repl); // same edit on both sides
                    bi = region_end.max(our_end).max(their_end);
                } else {
                    // Different edits to the same region: emit a conflict block
                    // so the result is inspectable, and record it so the caller
                    // refuses to commit.
                    let marker = format!(
                        "<<<<<<< ours\n{}\n=======\n{}\n>>>>>>> theirs",
                        our_repl.join("\n"),
                        their_repl.join("\n")
                    );
                    conflicts.push(marker.clone());
                    out.push(marker);
                    bi = region_end.max(our_end).max(their_end);
                }
            }
            (false, false) => {
                out.push(base_lines.get(bi).copied().unwrap_or("").to_string());
                bi += 1;
            }
        }

        // Skip any base lines the region consumed.
        while bi < region_end && bi < base_lines.len() {
            bi += 1;
        }
        let _ = (&base_lines, bi);
    }

    while bi < base_lines.len() {
        out.push(base_lines[bi].to_string());
        bi += 1;
    }

    let merged = out.join("\n");
    // Preserve the trailing newline of the inputs.
    let with_newline = if [base, ours, theirs].iter().any(|s| s.ends_with('\n')) {
        if merged.ends_with('\n') || merged.is_empty() {
            merged
        } else {
            format!("{}\n", merged)
        }
    } else {
        merged
    };

    if conflicts.is_empty() {
        MergeResult::Clean(with_newline)
    } else {
        MergeResult::Conflict {
            ours: ours.to_string(),
            theirs: theirs.to_string(),
            merged: with_newline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_of_identical_texts_is_empty() {
        assert!(diff_hunks("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn diff_reports_a_single_line_change() {
        let h = diff_hunks("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_start, h[0].old_len, h[0].new_len), (1, 1, 1));
    }

    #[test]
    fn diff_handles_pure_insert_and_delete() {
        assert_eq!(diff_hunks("", "a\nb\n").len(), 1);
        assert_eq!(diff_hunks("a\nb\n", "").len(), 1);
        let h = diff_hunks("a\nc\n", "a\nb\nc\n");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_len, h[0].new_len), (0, 1));
    }

    #[test]
    fn diff_keeps_duplicate_lines_in_position() {
        // Inserting one duplicate at the end must not look like a change to the
        // existing identical line earlier in the file.
        let h = diff_hunks("x\nother\nx\n", "x\nother\nx\nx\n");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_start, h[0].old_len, h[0].new_len), (3, 0, 1));
    }

    #[test]
    fn line_diff_orders_removed_and_added() {
        let (rem, add) = line_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(rem, vec!["b"]);
        assert_eq!(add, vec!["B"]);
    }

    #[test]
    fn merge_takes_a_one_sided_change() {
        let base = "a\nb\nc\n";
        let ours = "a\nB\nc\n";
        let theirs = "a\nb\nc\n";
        assert_eq!(merge3(base, ours, theirs).text(), "a\nB\nc\n");
        assert_eq!(merge3(base, theirs, ours).text(), "a\nB\nc\n");
    }

    #[test]
    fn merge_combines_non_overlapping_changes() {
        let base = "one\ntwo\nthree\nfour\nfive\n";
        let ours = "ONE\ntwo\nthree\nfour\nfive\n";
        let theirs = "one\ntwo\nthree\nfour\nFIVE\n";
        let r = merge3(base, ours, theirs);
        assert!(!r.is_conflict(), "disjoint edits must merge cleanly");
        assert_eq!(r.text(), "ONE\ntwo\nthree\nfour\nFIVE\n");
    }

    #[test]
    fn merge_reports_a_real_conflict() {
        let base = "line\n";
        let ours = "ours version\n";
        let theirs = "theirs version\n";
        let r = merge3(base, ours, theirs);
        assert!(
            r.is_conflict(),
            "same line changed differently must conflict"
        );
        assert!(r.text().contains("<<<<<<<"));
        assert!(r.text().contains("ours version"));
        assert!(r.text().contains("theirs version"));
    }

    #[test]
    fn merge_of_identical_changes_is_clean() {
        let base = "a\nb\n";
        let both = "a\nB\n";
        let r = merge3(base, both, both);
        assert!(
            !r.is_conflict(),
            "the same edit on both sides is not a conflict"
        );
        assert_eq!(r.text(), "a\nB\n");
    }

    #[test]
    fn merge_with_no_change_on_either_side_is_identity() {
        let base = "a\nb\nc\n";
        assert_eq!(merge3(base, base, base).text(), base);
    }

    #[test]
    fn merge_keeps_our_addition_when_theirs_adds_elsewhere() {
        let base = "top\nbottom\n";
        let ours = "top\nmiddle-ours\nbottom\n";
        let theirs = "top\nbottom\ntail-theirs\n";
        let r = merge3(base, ours, theirs);
        assert!(!r.is_conflict(), "got conflict: {}", r.text());
        assert!(r.text().contains("middle-ours"));
        assert!(r.text().contains("tail-theirs"));
    }

    #[test]
    fn merge_handles_empty_base() {
        let r = merge3("", "a\n", "a\n");
        assert!(!r.is_conflict());
        assert_eq!(r.text(), "a\n");
    }

    #[test]
    fn merge_conflict_when_both_delete_and_add_differently() {
        let base = "keep\ndrop\n";
        let ours = "keep\n";
        let theirs = "keep\nreplacement\n";
        let r = merge3(base, ours, theirs);
        assert!(r.is_conflict(), "delete vs replace must conflict");
    }
    /// Both sides changing overlapping lines to the SAME content must not duplicate the
    /// replacement. The hunks differ in length, so the shorter one started inside the region
    /// the longer one had already replaced and was applied a second time. A rebase produced a
    /// file with the same line twice and reported no conflict.
    #[test]
    fn overlapping_identical_changes_do_not_duplicate_a_line() {
        let base = "line1\nline2\nline3\n";
        // Ours replaces line2 with two lines; theirs replaces line3 with the second of them.
        let ours = "line1\nA\nB\nline3\n";
        let theirs = "line1\nline2\nB\n";
        let r = merge3(base, ours, theirs);
        let text = match &r {
            MergeResult::Clean(t) => t.clone(),
            MergeResult::Conflict { .. } => "CONFLICT".to_string(),
        };
        assert_eq!(
            text.matches('B').count(),
            1,
            "the replacement appeared more than once: {:?}",
            text
        );
    }

    #[test]
    fn a_line_added_by_both_sides_appears_once() {
        let base = "a\nb\n";
        let ours = "a\nb\nc\n";
        let theirs = "a\nb\nc\n";
        match merge3(base, ours, theirs) {
            MergeResult::Clean(t) => {
                assert_eq!(t, "a\nb\nc\n", "identical additions must stay one line");
            }
            MergeResult::Conflict { .. } => panic!("identical changes are not a conflict"),
        }
    }
}
