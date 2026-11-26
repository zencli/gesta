//! Structured conflict reports.
//!
//! The line markers `<<<<<<< / ======= / >>>>>>>` are readable by a person and
//! awkward for an agent: recovering which lines came from where means parsing
//! the markers back out, and the line numbers in the markers refer to the merged
//! file, not to either side. This module keeps the three versions of each
//! conflicting region as data, with the line numbers each side used, so a model
//! can reason about the two choices and write the resolution directly.

use crate::repo::merge::{merge3, MergeResult};

/// One conflicting region, as data.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictRegion {
    /// First affected line on our side, 1-based. 0 means "before line 1".
    pub ours_start: usize,
    /// Last affected line on our side, inclusive.
    pub ours_end: usize,
    /// First affected line on their side, 1-based.
    pub theirs_start: usize,
    pub theirs_end: usize,
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
    /// Unchanged lines directly above the region, to give a resolver context.
    pub context_before: Vec<String>,
    /// Unchanged lines directly below the region.
    pub context_after: Vec<String>,
}

/// Everything a caller needs to resolve one file.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictReport {
    pub path: String,
    /// The merged text with markers, in case a human wants to look at it.
    pub merged: String,
    pub regions: Vec<ConflictRegion>,
    /// The whole file on each side, so a resolver can choose per region or take
    /// one side wholesale without asking for another read.
    pub ours: String,
    pub theirs: String,
    pub base: String,
}

/// Number of unchanged lines included either side of a conflicting region.
/// Three is what a diff tool shows by default and what makes a decision
/// possible without loading the whole file.
const CONTEXT_LINES: usize = 3;

/// Build a report for one file, or `None` when the merge is clean.
pub fn report(path: &str, base: &str, ours: &str, theirs: &str) -> Option<ConflictReport> {
    let result = merge3(base, ours, theirs);
    let merged = match result {
        MergeResult::Clean(_) => return None,
        MergeResult::Conflict { merged, .. } => merged,
    };

    let our_lines: Vec<String> = ours.lines().map(|s| s.to_string()).collect();
    let their_lines: Vec<String> = theirs.lines().map(|s| s.to_string()).collect();
    let regions = split_regions(&merged, &our_lines, &their_lines);

    Some(ConflictReport {
        path: path.to_string(),
        merged,
        regions,
        ours: ours.to_string(),
        theirs: theirs.to_string(),
        base: base.to_string(),
    })
}

/// Walk the marked-up text and cut it into regions.
///
/// The markers carry no line numbers of their own, so the line numbers are
/// recovered by matching each side's block back to the source lines. A block
/// that appears more than once resolves to the first match; that is a known
/// limit of marker-based output and the reason the region carries the text
/// itself, which is the part a resolver actually needs.
fn split_regions(
    merged: &str,
    our_lines: &[String],
    their_lines: &[String],
) -> Vec<ConflictRegion> {
    let mut regions = Vec::new();
    let lines: Vec<&str> = merged.lines().collect();
    let mut i = 0usize;
    let mut cursor_ours = 0usize; // how far into our file we have accounted for

    while i < lines.len() {
        if !lines[i].starts_with("<<<<<<<") {
            // Ordinary merged line. Advance our cursor when it also exists in
            // our version, which keeps the numbering honest for the next region.
            if cursor_ours < our_lines.len() && our_lines[cursor_ours] == lines[i] {
                cursor_ours += 1;
            }
            i += 1;
            continue;
        }

        let mut our_block: Vec<String> = Vec::new();
        let mut their_block: Vec<String> = Vec::new();
        i += 1;
        while i < lines.len() && !lines[i].starts_with("=======") {
            our_block.push(lines[i].to_string());
            i += 1;
        }
        i += 1; // skip =======
        while i < lines.len() && !lines[i].starts_with(">>>>>>>") {
            their_block.push(lines[i].to_string());
            i += 1;
        }
        i += 1; // skip >>>>>>>

        // The blank line the marker format puts before `>>>>>>>` when their side is empty parses as
        // one empty string. That is the marker's own shape, not a line of theirs, and leaving it in
        // made an empty side look like a one-line region containing "".
        if their_block.len() == 1 && their_block[0].is_empty() {
            their_block.clear();
        }
        if our_block.len() == 1 && our_block[0].is_empty() {
            our_block.clear();
        }

        // Where our side's region ends is not `cursor + block.len()`: the marker block holds the
        // lines the merge chose to show, and the unchanged lines inside the changed range are not in
        // it. The module's own docstring says the numbers are recovered by matching back to the
        // source, so the next ordinary merged line is looked up in our file and the region ends
        // where that line begins. Without this a three-line change reported a one-line range and
        // `context_after` began inside the region.
        let next_ordinary = lines[i..].iter().find(|l| !is_marker(l));
        let our_end_idx = match next_ordinary {
            Some(next) => our_lines
                .iter()
                .enumerate()
                .skip(cursor_ours)
                .find(|(_, l)| l == next)
                .map(|(idx, _)| idx)
                .unwrap_or(our_lines.len()),
            None => our_lines.len(),
        };

        // An empty side has no lines, so it has no range. The old code produced
        // `ours_start = cursor + 1, ours_end = cursor`, an inverted range that a reader has to
        // notice is backwards, and on their side it fell back to our position, pointing at a line
        // that belongs to the other version.
        //
        // The encoding here is `start = end + 1` on the empty side: the region begins where the
        // next line would be, and reading it as a half-open range yields nothing. `0` stays reserved
        // for "before line 1", which the module already documents on `ours_start`.
        let (ours_start, ours_end) = if our_end_idx > cursor_ours {
            (cursor_ours + 1, our_end_idx)
        } else {
            // Our side deleted the lines too: no range, and the start says where they would be.
            (cursor_ours + 1, cursor_ours)
        };

        let (theirs_start, theirs_end) = if their_block.is_empty() {
            // Their side has nothing here. The position is where our side's region starts, which is
            // the only anchor available, and the end being one less than the start says "no lines"
            // rather than "one line here".
            (ours_start, ours_start.saturating_sub(1))
        } else {
            match find_block(their_lines, &their_block) {
                Some(p) => {
                    let start = p + 1;
                    (start, start + their_block.len() - 1)
                }
                // The block is not in their file at all. Naming a position there would be a lie, so
                // the range is empty and `theirs` still carries the lines.
                None => (0, 0),
            }
        };

        // Context comes from our side, which is what the working tree holds.
        let before_from = cursor_ours.saturating_sub(CONTEXT_LINES);
        let context_before = our_lines[before_from..cursor_ours].to_vec();
        let after_to = (ours_end + CONTEXT_LINES).min(our_lines.len());
        let context_after = if ours_end < our_lines.len() {
            our_lines[ours_end..after_to].to_vec()
        } else {
            Vec::new()
        };

        regions.push(ConflictRegion {
            ours_start,
            ours_end,
            theirs_start,
            theirs_end,
            ours: our_block.clone(),
            theirs: their_block,
            context_before,
            context_after,
        });

        // The cursor moves to where the region ended, not by the marker block's length: the block
        // holds only the lines the merge chose to show, and the unchanged lines inside a changed
        // range are not in it.
        cursor_ours = our_end_idx.max(cursor_ours);
    }

    regions
}

/// Whether a merged line is one of the conflict markers rather than file content.
fn is_marker(line: &str) -> bool {
    line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
}

/// First position where `block` appears in `hay`, as a 0-based index.
fn find_block(hay: &[String], block: &[String]) -> Option<usize> {
    if block.is_empty() || block.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - block.len()).find(|&start| hay[start..start + block.len()] == *block)
}

/// Render a report as JSON, the shape an agent consumes.
pub fn to_json(r: &ConflictReport) -> crate::support::json::Json {
    use crate::support::json::Json;

    let regions = r
        .regions
        .iter()
        .map(|g| {
            Json::obj(vec![
                ("ours_start", Json::int(g.ours_start as i64)),
                ("ours_end", Json::int(g.ours_end as i64)),
                ("theirs_start", Json::int(g.theirs_start as i64)),
                ("theirs_end", Json::int(g.theirs_end as i64)),
                (
                    "ours",
                    Json::arr(g.ours.iter().cloned().map(Json::str).collect()),
                ),
                (
                    "theirs",
                    Json::arr(g.theirs.iter().cloned().map(Json::str).collect()),
                ),
                (
                    "context_before",
                    Json::arr(g.context_before.iter().cloned().map(Json::str).collect()),
                ),
                (
                    "context_after",
                    Json::arr(g.context_after.iter().cloned().map(Json::str).collect()),
                ),
            ])
        })
        .collect();

    Json::obj(vec![
        ("path", Json::str(r.path.clone())),
        ("conflict_count", Json::int(r.regions.len() as i64)),
        ("regions", Json::arr(regions)),
        ("merged", Json::str(r.merged.clone())),
        ("ours", Json::str(r.ours.clone())),
        ("theirs", Json::str(r.theirs.clone())),
        ("base", Json::str(r.base.clone())),
    ])
}

/// Render a whole set of reports as one JSON document.
pub fn reports_to_json(reports: &[ConflictReport]) -> crate::support::json::Json {
    use crate::support::json::Json;
    Json::obj(vec![
        ("conflict_count", Json::int(reports.len() as i64)),
        ("files", Json::arr(reports.iter().map(to_json).collect())),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "alpha\nbeta\ngamma\n";

    #[test]
    fn a_clean_merge_produces_no_report() {
        let r = report("f.txt", BASE, "alpha\nbeta\ngamma\n", "alpha\nbeta gamma\n");
        assert!(r.is_none(), "no conflict means nothing to report");
    }

    #[test]
    fn a_conflict_names_both_sides_as_lines() {
        let ours = "alpha\nBETA\ngamma\n";
        let theirs = "alpha\nbeta2\ngamma\n";
        let r = report("f.txt", BASE, ours, theirs).expect("expected a conflict");
        assert_eq!(r.regions.len(), 1);
        assert_eq!(r.regions[0].ours, vec!["BETA".to_string()]);
        assert_eq!(r.regions[0].theirs, vec!["beta2".to_string()]);
    }

    #[test]
    fn a_region_carries_line_numbers_for_both_sides() {
        let ours = "alpha\nBETA\ngamma\n";
        let theirs = "alpha\nbeta2\ngamma\n";
        let r = report("f.txt", BASE, ours, theirs).unwrap();
        let g = &r.regions[0];
        assert_eq!((g.ours_start, g.ours_end), (2, 2));
        assert_eq!(g.theirs, vec!["beta2".to_string()]);
        assert!(g.theirs_start >= 1);
    }

    #[test]
    fn a_region_carries_context_so_a_resolver_can_judge() {
        let base = "l1\nl2\nl3\nl4\nl5\nl6\nl7\n";
        let ours = "l1\nl2\nl3\nOURS\nl5\nl6\nl7\n";
        let theirs = "l1\nl2\nl3\nTHEIRS\nl5\nl6\nl7\n";
        let r = report("f.txt", base, ours, theirs).unwrap();
        let g = &r.regions[0];
        assert_eq!(g.context_before, vec!["l1", "l2", "l3"]);
        assert_eq!(g.context_after, vec!["l5", "l6", "l7"]);
    }

    #[test]
    fn both_whole_files_are_included_for_a_wholesale_choice() {
        let ours = "alpha\nBETA\ngamma\n";
        let theirs = "alpha\nbeta2\ngamma\n";
        let r = report("f.txt", BASE, ours, theirs).unwrap();
        assert_eq!(r.ours, ours);
        assert_eq!(r.theirs, theirs);
        assert_eq!(r.base, BASE);
    }

    #[test]
    fn two_separate_conflicts_produce_two_regions() {
        let base = "a\nb\nc\nd\ne\nf\ng\n";
        let ours = "a\nB\nc\nd\nE\nf\ng\n";
        let theirs = "a\nB2\nc\nd\nE2\nf\ng\n";
        let r = report("f.txt", base, ours, theirs).unwrap();
        assert_eq!(r.regions.len(), 2, "got {:?}", r.regions);
        assert_eq!(r.regions[0].ours, vec!["B".to_string()]);
        assert_eq!(r.regions[1].ours, vec!["E".to_string()]);
    }

    #[test]
    fn a_region_where_they_deleted_lines_still_reports_our_text() {
        let base = "keep\ndrop me\nkeep2\n";
        let ours = "keep\ndrop me\nkeep2\n";
        let theirs = "keep\nkeep2\n";
        let r = report("f.txt", base, ours, theirs);
        // One side unchanged and the other deleting is a clean merge.
        assert!(
            r.is_none(),
            "a deletion with no opposing edit merges cleanly"
        );
    }

    #[test]
    fn the_report_is_serialisable_to_json() {
        let ours = "alpha\nBETA\ngamma\n";
        let theirs = "alpha\nbeta2\ngamma\n";
        let r = report("f.txt", BASE, ours, theirs).unwrap();
        let j = crate::repo::conflict::to_json(&r);
        let text = j.render();
        assert!(text.contains("\"path\":\"f.txt\""));
        assert!(text.contains("\"regions\""));
        assert!(text.contains("BETA"));
        assert!(text.contains("beta2"));
    }
}
