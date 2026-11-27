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

/// Pairs of matching lines found by walking both sides once.
///
/// This is the path taken when the full table would be too large. It matches greedily in order: the
/// first line of `a` that still appears ahead in `b` is paired with that position. That is exactly
/// right for the case the ceiling is meant to serve - large generated files where most lines are
/// unique - and it is linear in both inputs. Lines that repeat many times can pair differently here
/// than the table would, so the alignment may be less pretty; it is never wrong about which lines
/// are equal, which is what callers read it for.
fn anchor_matches(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    use std::collections::VecDeque;
    let mut positions: std::collections::HashMap<&str, VecDeque<usize>> =
        std::collections::HashMap::new();
    for (j, line) in b.iter().enumerate() {
        positions.entry(line).or_default().push_back(j);
    }
    let mut out = Vec::new();
    let mut next_after = 0usize;
    for (i, line) in a.iter().enumerate() {
        if let Some(q) = positions.get_mut(line) {
            while q.front().is_some_and(|&j| j < next_after) {
                q.pop_front();
            }
            if let Some(j) = q.pop_front() {
                out.push((i, j));
                next_after = j + 1;
            }
        }
    }
    out
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
        // The table is (n+1)*(m+1) u32s. Two files of 50,000 changed lines ask for 10 GB, and the
        // allocation happens before any decision, so a repository can be taken down by one commit
        // rather than by a crafted one: a large generated file is enough. The ceiling keeps the
        // cost at 64 MB; past it, matching falls back to an anchor walk that is linear and gives a
        // correct alignment for everything except lines that repeat many times.
        const MAX_CELLS: usize = 16 * 1024 * 1024;
        let cells = n.saturating_add(1).saturating_mul(m.saturating_add(1));
        if cells > MAX_CELLS {
            let mut out: Vec<(usize, usize)> = (0..pre).map(|i| (i, i)).collect();
            out.extend(
                anchor_matches(a_mid, b_mid)
                    .into_iter()
                    .map(|(i, j)| (pre + i, pre + j)),
            );
            out.extend((0..suf).map(|k| (a.len() - suf + k, b.len() - suf + k)));
            return out;
        }
        // One flat buffer instead of a Vec of Vecs: the row vector is what made this table cost a
        // pointer indirection per row and an allocation per row, and the flat form makes the
        // ceiling above a simple product rather than a sum of allocations.
        let stride = m + 1;
        let mut dp = vec![0u32; (n + 1) * stride];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * stride + j] = if a_mid[i] == b_mid[j] {
                    dp[(i + 1) * stride + j + 1] + 1
                } else {
                    dp[(i + 1) * stride + j].max(dp[i * stride + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < n && j < m {
            if a_mid[i] == b_mid[j] {
                mid.push((pre + i, pre + j));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * stride + j] >= dp[i * stride + j + 1] {
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

        // A hunk still to be applied that starts before `bi` covers base lines this walk has already
        // emitted: the other side's region ended past its start, so the two sides changed the same
        // base lines. That is a conflict, and it used to be applied as if it sat after the emitted
        // text, silently dropping the base lines both sides had replaced. Base `1,2,3,4` with ours
        // changing `1,2` and theirs `2,3` came out as `A,B,X,Y,4` with no conflict reported: both `2`
        // and `3` were gone.
        //
        // The skip loops above only consume a hunk that ends at or before `bi`. One that starts
        // before it and ends after is exactly this case, and it never was handled.
        let overlaps = |h: Option<&Hunk>| {
            h.map(|h| h.old_len > 0 && h.old_start < bi)
                .unwrap_or(false)
        };
        if overlaps(our_hunks.get(oi)) || overlaps(their_hunks.get(ti)) {
            let (o_start, o_end, o_repl) = match our_hunks.get(oi) {
                Some(h) => (
                    h.old_start,
                    h.old_start + h.old_len,
                    our_lines[h.new_start..h.new_start + h.new_len]
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>(),
                ),
                None => (bi, bi, Vec::new()),
            };
            let (t_start, t_end, t_repl) = match their_hunks.get(ti) {
                Some(h) => (
                    h.old_start,
                    h.old_start + h.old_len,
                    their_lines[h.new_start..h.new_start + h.new_len]
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>(),
                ),
                None => (bi, bi, Vec::new()),
            };
            // Whatever base lines sit between what has been emitted and the earlier of the two
            // regions are untouched by either side and belong in the output before the marker.
            let mark = next_base.max(bi).max(o_start.min(t_start));
            while bi < mark && bi < base_lines.len() {
                out.push(base_lines[bi].to_string());
                bi += 1;
            }
            // Overlapping edits that produce the SAME text are not a conflict (#280).
            //
            // The branch above emitted a marker unconditionally, so base `1,2,3,4` with ours changing
            // `1,2` to `X` and theirs changing `2,3` to `X` came out as a conflict whose ours side was
            // empty - even though both sides agreed on the answer, which is `X 4`. A conflict marker
            // stops the merge and asks a person to resolve something already resolved, and it was
            // reported for a case with one correct outcome.
            //
            // The comparison is between the two replacements, not between ranges: the ranges differ by
            // construction here, which is why this branch was reached.
            if o_repl == t_repl {
                out.extend(o_repl);
                bi = o_end.max(t_end).max(bi);
                if o_start <= t_start {
                    oi += 1;
                }
                if t_start <= o_start {
                    ti += 1;
                }
                continue;
            }
            let marker = format!(
                "<<<<<<< ours\n{}\n=======\n{}\n>>>>>>> theirs",
                o_repl.join("\n"),
                t_repl.join("\n")
            );
            conflicts.push(marker.clone());
            out.push(marker);
            bi = o_end.max(t_end).max(bi);
            if o_start <= t_start {
                oi += 1;
            }
            if t_start <= o_start {
                ti += 1;
            }
            continue;
        }

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
        // At least one of their hunks here adds lines rather than replacing them.
        let mut their_repl_is_insertion = false;
        while ti < their_hunks.len() && their_hunks[ti].old_start == next_base {
            let h = &their_hunks[ti];
            their_end = h.old_start + h.old_len;
            their_repl = their_lines[h.new_start..h.new_start + h.new_len]
                .iter()
                .map(|s| s.to_string())
                .collect();
            their_changed = true;
            if h.old_len == 0 {
                their_repl_is_insertion = true;
            }
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
                // The tail comparison answers "is my replacement already the end of the file",
                // which is only a duplicate when this hunk replaces base lines. A pure insertion
                // adds lines that were not there: base `a,a` with theirs `a,a,a` appends one `a`,
                // the file already ends with `a`, and the append was dropped while the merge
                // reported success. Insertions are therefore never suppressed this way.
                // The tail-dedup that used to live here compared `their_repl` against the end of
                // `out` and dropped it on a match (#269). That is wrong whenever the two hunks
                // replace DIFFERENT base lines: base `m,n` with ours changing `m` to `K` and theirs
                // changing `n` to `K` produced `K` once, because the second `K` matched the tail the
                // first one had just emitted. Two independent lines, two independent replacements, and
                // the answer is `K\nK\n`.
                //
                // What the dedup was for is the case where BOTH sides make the same edit to the SAME
                // base lines - base `a,a` with both sides appending `a` - and that case never reaches
                // this branch: identical replacements to an identical region are handled by the
                // `(true, true)` arm above, which compares `our_repl == their_repl`. A hunk whose base
                // lines differ from what was just emitted is a real edit and is kept.
                //
                // So there is nothing left to suppress here. The insertion exemption from #122 stays,
                // because it documents why this branch appends rather than dedups.
                let _ = their_repl_is_insertion;
                out.extend(their_repl);
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
    fn two_large_rewrites_do_not_allocate_a_table_that_cannot_fit() {
        // The table used to be (n+1)*(m+1) u32s allocated before any decision. Two files of 50,000
        // changed lines ask for 10 GB, so one commit with a large generated file could take the
        // process down - not a crafted input, just a big one. This drives the fallback path and
        // checks it stays inside a sane time and memory budget.
        let old: String = (0..50_000).map(|i| format!("old {}\n", i)).collect();
        let new: String = (0..50_000).map(|i| format!("new {}\n", i)).collect();
        let started = std::time::Instant::now();
        let hunks = diff_hunks(&old, &new);
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(20),
            "diffing two 50k-line rewrites took {took:?}"
        );
        // Nothing is common, so the whole file is one replaced region.
        assert_eq!(hunks.len(), 1, "expected one hunk, got {:?}", hunks.len());
        assert_eq!(hunks[0].old_len, 50_000);
        assert_eq!(hunks[0].new_len, 50_000);
    }

    #[test]
    fn a_small_edit_in_a_large_file_still_reports_one_hunk() {
        // The fallback has to be right about the common case too: one changed line in a big file is
        // what the prefix/suffix stripping exists for, and it must not turn into a whole-file hunk.
        let old: String = (0..40_000).map(|i| format!("line {}\n", i)).collect();
        let mut new: String = (0..40_000).map(|i| format!("line {}\n", i)).collect();
        new = new.replace("line 20000\n", "line 20000 changed\n");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(hunks.len(), 1, "expected one hunk, got {}", hunks.len());
        assert_eq!(
            hunks[0].old_len, 1,
            "a one-line edit reported {} old lines",
            hunks[0].old_len
        );
        assert_eq!(hunks[0].new_len, 1);
    }

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
    fn overlapping_replacements_of_different_lines_both_land() {
        // Ours replaces line2 with two lines; theirs replaces line3 with the second of them.
        //
        // This test used to assert that `B` appeared ONCE, which is the bug #269 reports: line2 and
        // line3 are different base lines, so theirs replacing line3 with `B` is an edit in its own
        // right and both `B`s belong in the result. The expectation was changed rather than the code
        // because a three-way merge of two independent edits to two independent lines has exactly one
        // correct answer, and it is `line1 A B B`.
        let base = "line1\nline2\nline3\n";
        let ours = "line1\nA\nB\nline3\n";
        let theirs = "line1\nline2\nB\n";
        let r = merge3(base, ours, theirs);
        let text = match &r {
            MergeResult::Clean(t) => t.clone(),
            MergeResult::Conflict { .. } => "CONFLICT".to_string(),
        };
        assert_eq!(text, "line1\nA\nB\nB\n", "got: {text:?}");
    }

    #[test]
    fn two_sides_changing_overlapping_base_lines_conflict_instead_of_dropping_them() {
        // #119: base `1,2,3,4`, ours changes `1,2`, theirs changes `2,3`. Line 2 is changed by both.
        // This used to come out Clean("A\nB\nX\nY\n4\n"): both `2` and `3` were gone and neither
        // side's edit to the shared line survived, with no conflict reported.
        let r = merge3("1\n2\n3\n4\n", "A\nB\n3\n4\n", "1\nX\nY\n4\n");
        assert!(
            r.is_conflict(),
            "overlapping edits must conflict, got {:?}",
            r.text()
        );
        let text = r.text();
        // Both sides are in the marker, so a person resolving it can see what they are choosing.
        assert!(text.contains("<<<<<<< ours"), "{text}");
        assert!(
            text.contains("A") && text.contains("B"),
            "our side missing: {text}"
        );
        assert!(
            text.contains("X") && text.contains("Y"),
            "their side missing: {text}"
        );
    }

    #[test]
    fn a_region_that_merely_touches_the_next_one_does_not_conflict() {
        // Adjacent, not overlapping: ours changes line 1, theirs changes line 2. The guard must key
        // on overlap, not on being near.
        let r = merge3("1\n2\n3\n", "A\n2\n3\n", "1\nB\n3\n");
        assert!(
            !r.is_conflict(),
            "adjacent edits are not a conflict: {:?}",
            r.text()
        );
        let t = r.text();
        assert!(t.contains("A") && t.contains("B") && t.contains("3"), "{t}");
    }

    #[test]
    fn a_one_sided_append_of_a_duplicate_line_is_kept() {
        // #122: base `a,a`, theirs `a,a,a` appends one `a`. The duplicate check compared the tail of
        // the output against their replacement, saw the file already ended with `a`, and dropped the
        // append while reporting success.
        let r = merge3("a\na\n", "a\na\n", "a\na\na\n");
        match r {
            MergeResult::Clean(t) => assert_eq!(
                t, "a\na\na\n",
                "a one-sided append of a duplicate line must be kept"
            ),
            MergeResult::Conflict { .. } => panic!("a one-sided append is not a conflict"),
        }

        // The same shape where the repeated line is not at the end of the base.
        let r2 = merge3("x\ny\n", "x\ny\n", "x\ny\ny\n");
        match r2 {
            MergeResult::Clean(t) => assert_eq!(t, "x\ny\ny\n"),
            MergeResult::Conflict { .. } => panic!("a one-sided append is not a conflict"),
        }
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
    #[test]
    fn two_sides_replacing_different_lines_with_the_same_text_keep_both() {
        // #269: base `m,n`; ours changes `m` to `K`; theirs changes `n` to `K`. Two independent base
        // lines, two independent replacements, and the answer is `K\nK\n`. The tail-dedup in the
        // `(false, true)` branch compared `their_repl` against the end of what had been emitted and
        // suppressed it, so one side's edit vanished while the merge reported Clean.
        assert_eq!(
            merge3("m\nn\n", "K\nn\n", "m\nK\n").text(),
            "K\nK\n",
            "two different base lines replaced by the same text stay two lines"
        );
    }

    #[test]
    fn a_replacement_matching_the_emitted_tail_of_a_different_region_is_not_suppressed() {
        // The same shape as the case above with a third line in play, so the suppressed hunk is not
        // adjacent to the one that produced the matching tail.
        assert_eq!(
            merge3("a\nm\nn\n", "a\nK\nn\n", "a\nm\nK\n").text(),
            "a\nK\nK\n"
        );
    }
}
