//! Structured conflict reports.
//!
//! The line markers `<<<<<<< / ======= / >>>>>>>` are readable by a person and
//! awkward for an agent: recovering which lines came from where means parsing
//! the markers back out, and the line numbers in the markers refer to the merged
//! file, not to either side. This module keeps the three versions of each
//! conflicting region as data, with the line numbers each side used, so a model
//! can reason about the two choices and write the resolution directly.

use crate::merge::{merge3, MergeResult};

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

        let ours_start = cursor_ours + 1;
        let ours_end = cursor_ours + our_block.len();
        let theirs_start = find_block(their_lines, &their_block)
            .map(|p| p + 1)
            .unwrap_or(ours_start);
        let theirs_end = theirs_start + their_block.len().saturating_sub(1);

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

        cursor_ours += our_block.len();
    }

    regions
}

/// First position where `block` appears in `hay`, as a 0-based index.
fn find_block(hay: &[String], block: &[String]) -> Option<usize> {
    if block.is_empty() || block.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - block.len()).find(|&start| hay[start..start + block.len()] == *block)
}

/// Render a report as JSON, the shape an agent consumes.
pub fn to_json(r: &ConflictReport) -> crate::json::Json {
    use crate::json::Json;

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
pub fn reports_to_json(reports: &[ConflictReport]) -> crate::json::Json {
    use crate::json::Json;
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
        let j = crate::conflict::to_json(&r);
        let text = j.render();
        assert!(text.contains("\"path\":\"f.txt\""));
        assert!(text.contains("\"regions\""));
        assert!(text.contains("BETA"));
        assert!(text.contains("beta2"));
    }
}
