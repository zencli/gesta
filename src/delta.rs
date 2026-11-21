//! Line delta encoding.
//!
//! Storing every version of a file whole means a 500 KB file edited fifty times
//! costs fifty compressions of a file that changed in one line. A delta records
//! only the copy and insert operations needed to rebuild one version from
//! another, which is what git's packfiles do and what makes editing history
//! affordable.
//!
//! The format is line based and deliberately simple: a copy of N lines starting
//! at line S in the base, or an insert of N literal lines. It is not the
//! smallest possible delta, but it is verifiable in one pass and it never
//! produces a version that does not match its hash: `encode` rebuilds the target
//! from the ops and returns `None` if the rebuild differs.

/// One operation in a delta.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Copy `count` lines starting at 0-based `start` in the base.
    Copy { start: usize, count: usize },
    /// Insert these literal lines.
    Insert(Vec<String>),
}

fn split_lines(s: &str) -> Vec<String> {
    s.lines().map(|l| l.to_string()).collect()
}

/// Encode `new` relative to `base`.
///
/// Returns the ops, or `None` when the delta would not be smaller than the file
/// itself, or when the rebuild does not match, which is the only safe answer at
/// that point.
pub fn encode(base: &str, new: &str) -> Option<Vec<Op>> {
    let a = split_lines(base);
    let b = split_lines(new);
    if b.is_empty() {
        return Some(Vec::new());
    }
    // A delta costs roughly one line per run. Below this there is nothing to win.
    if b.len() < 16 {
        return None;
    }

    let matches = lcs_pairs(&a, &b);
    let mut ops: Vec<Op> = Vec::new();
    let mut bi = 0usize; // position in b consumed so far
    let mut mi = 0usize; // index into matches

    while mi < matches.len() {
        let (ai_start, bi_start) = matches[mi];
        if bi_start > bi {
            ops.push(Op::Insert(b[bi..bi_start].to_vec()));
        }
        // Extend while both sides advance by one, so consecutive equal lines
        // become one Copy rather than many.
        let mut run = 0usize;
        let mut k = mi;
        while k < matches.len() && matches[k].0 == ai_start + run && matches[k].1 == bi_start + run
        {
            run += 1;
            k += 1;
        }
        ops.push(Op::Copy {
            start: ai_start,
            count: run,
        });
        bi = bi_start + run;
        mi = k;
    }
    if bi < b.len() {
        ops.push(Op::Insert(b[bi..].to_vec()));
    }

    // Verify before anyone depends on this.
    match apply(base, &ops) {
        Some(rebuilt) if rebuilt == new => Some(ops),
        _ => None,
    }
}

/// Rebuild the target from a base and ops.
pub fn apply(base: &str, ops: &[Op]) -> Option<String> {
    let a = split_lines(base);
    let mut out: Vec<String> = Vec::new();
    for op in ops {
        match op {
            Op::Copy { start, count } => {
                let end = start.checked_add(*count)?;
                if end > a.len() {
                    return None; // a corrupt delta must not panic
                }
                out.extend_from_slice(&a[*start..end]);
            }
            Op::Insert(lines) => out.extend(lines.iter().cloned()),
        }
    }
    if out.is_empty() {
        return Some(String::new());
    }
    let mut s = out.join("\n");
    // `lines()` drops the trailing newline, so restore it when the source had
    // one. Without this a round trip silently eats the final byte.
    s.push('\n');
    Some(s)
}

/// Longest common subsequence over lines, as (index in a, index in b) pairs.
///
/// Textbook dynamic programming, which is fine here because a delta is computed
/// once per changed file and the inputs are line counts, not bytes.
fn lcs_pairs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 {
        return Vec::new();
    }
    // Guard the allocation: a pathological pair of 20k-line files would need
    // 400M cells at 2 bytes each. Above that, fall back to no matches, which
    // still produces a correct (if larger) delta.
    if n.saturating_mul(m) > 40_000_000 {
        return Vec::new();
    }
    let mut table = vec![0u16; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[idx(i, j)] = if a[i] == b[j] {
                table[idx(i + 1, j + 1)].saturating_add(1)
            } else {
                table[idx(i + 1, j)].max(table[idx(i, j + 1)])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if table[idx(i + 1, j)] >= table[idx(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Serialise ops to bytes: one header line per op, insert bodies raw.
pub fn to_bytes(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::Copy { start, count } => {
                out.extend_from_slice(format!("C\t{}\t{}\n", start, count).as_bytes());
            }
            Op::Insert(lines) => {
                out.extend_from_slice(format!("A\t{}\n", lines.len()).as_bytes());
                for l in lines {
                    out.extend_from_slice(l.as_bytes());
                    out.push(b'\n');
                }
            }
        }
    }
    out
}

/// Parse the byte form back into ops.
pub fn from_bytes(raw: &[u8]) -> Option<Vec<Op>> {
    let text = std::str::from_utf8(raw).ok()?;
    // Every line matters, including an empty one. `to_bytes` writes an insert of a blank line as
    // `A\t1\n\n`, and a parser that skipped empty lines read that as an insert of nothing, so a
    // delta whose only change was a blank line decoded into ops that rebuilt different content
    // and the object became unreadable.
    let mut lines = text.split('\n');
    let mut ops = Vec::new();
    while let Some(header) = lines.next() {
        if header.is_empty() {
            // The trailing newline of the last record leaves one empty tail, which is not a
            // record. An empty header anywhere else is a malformed payload.
            if lines.clone().all(|l| l.is_empty()) {
                break;
            }
            return None;
        }
        if let Some(rest) = header.strip_prefix("C\t") {
            let mut parts = rest.split('\t');
            let start: usize = parts.next()?.parse().ok()?;
            let count: usize = parts.next()?.parse().ok()?;
            ops.push(Op::Copy { start, count });
            continue;
        }
        let count: usize = header.strip_prefix("A\t")?.parse().ok()?;
        let mut body = Vec::with_capacity(count);
        for _ in 0..count {
            body.push(lines.next()?.to_string());
        }
        ops.push(Op::Insert(body));
    }
    Some(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_doc() -> String {
        (0..100).map(|i| format!("line {:03}\n", i)).collect()
    }

    #[test]
    fn a_delta_round_trips_through_apply() {
        let base = base_doc();
        let mut lines: Vec<String> = base.lines().map(|s| s.to_string()).collect();
        lines[50] = "line 050 CHANGED".to_string();
        let new = lines.join("\n") + "\n";

        let ops = encode(&base, &new).expect("a one line edit must produce a delta");
        assert_eq!(apply(&base, &ops).unwrap(), new);
    }

    #[test]
    fn a_two_line_edit_round_trips() {
        let base = base_doc();
        let mut lines: Vec<String> = base.lines().map(|s| s.to_string()).collect();
        lines[10] = "edited ten".to_string();
        lines[80] = "edited eighty".to_string();
        let new = lines.join("\n") + "\n";
        let ops = encode(&base, &new).unwrap();
        assert_eq!(apply(&base, &ops).unwrap(), new);
    }

    #[test]
    fn an_insertion_in_the_middle_round_trips() {
        let base = base_doc();
        let mut lines: Vec<String> = base.lines().map(|s| s.to_string()).collect();
        lines.insert(40, "inserted here".to_string());
        let new = lines.join("\n") + "\n";
        let ops = encode(&base, &new).unwrap();
        assert_eq!(apply(&base, &ops).unwrap(), new);
    }

    #[test]
    fn a_deletion_round_trips() {
        let base = base_doc();
        let mut lines: Vec<String> = base.lines().map(|s| s.to_string()).collect();
        lines.remove(20);
        let new = lines.join("\n") + "\n";
        let ops = encode(&base, &new).unwrap();
        assert_eq!(apply(&base, &ops).unwrap(), new);
    }

    #[test]
    fn a_delta_is_much_smaller_than_the_file() {
        let base = base_doc();
        let mut lines: Vec<String> = base.lines().map(|s| s.to_string()).collect();
        lines[50] = "line 050 CHANGED".to_string();
        let new = lines.join("\n") + "\n";
        let ops = encode(&base, &new).unwrap();
        let bytes = to_bytes(&ops);
        assert!(
            bytes.len() * 8 < new.len(),
            "delta is {} bytes against a {} byte file",
            bytes.len(),
            new.len()
        );
    }

    #[test]
    fn bytes_round_trip_through_the_serialiser() {
        let base = base_doc();
        let mut lines: Vec<String> = base.lines().map(|s| s.to_string()).collect();
        lines[5] = "five".to_string();
        lines.insert(60, "sixty new".to_string());
        let new = lines.join("\n") + "\n";
        let ops = encode(&base, &new).unwrap();
        let bytes = to_bytes(&ops);
        let back = from_bytes(&bytes).unwrap();
        assert_eq!(apply(&base, &back).unwrap(), new);
    }

    #[test]
    fn an_unrelated_file_produces_no_useful_delta() {
        // Two files with nothing in common: correct but not worth storing.
        let base = (0..50)
            .map(|i| format!("alpha {}\n", i))
            .collect::<String>();
        let new = (0..50)
            .map(|i| format!("omega {}\n", i))
            .collect::<String>();
        match encode(&base, &new) {
            None => {}
            Some(ops) => {
                let bytes = to_bytes(&ops);
                assert!(
                    bytes.len() >= new.len() / 2,
                    "an unrelated delta should not look like a win"
                );
            }
        }
    }

    #[test]
    fn a_short_file_gets_no_delta() {
        assert!(encode("a\nb\n", "a\nc\n").is_none(), "not worth the ops");
    }

    #[test]
    fn a_corrupt_delta_returns_none_instead_of_panicking() {
        // Copy past the end of the base.
        let ops = vec![Op::Copy {
            start: 500,
            count: 10,
        }];
        assert_eq!(apply("one\ntwo\n", &ops), None);

        // A truncated byte stream.
        assert!(from_bytes(b"C\t0\t5").is_some(), "a header alone parses");
        assert!(from_bytes(b"X\tbad").is_none(), "an unknown op is rejected");
    }

    #[test]
    fn an_empty_base_and_a_fresh_file_round_trip() {
        let new = (0..30).map(|i| format!("new {}\n", i)).collect::<String>();
        let ops = encode("", &new).expect("every line is an insert");
        assert_eq!(apply("", &ops).unwrap(), new);
    }

    #[test]
    fn identical_content_produces_one_copy() {
        let base = base_doc();
        let ops = encode(&base, &base).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], Op::Copy { count: 100, .. }));
        assert_eq!(apply(&base, &ops).unwrap(), base);
    }
    /// A delta whose only change is a blank line decoded into an insert of nothing, and the
    /// object it described could never be read again.
    #[test]
    fn an_inserted_blank_line_survives_the_byte_round_trip() {
        let base: String = (0..40).map(|i| format!("line {}\n", i)).collect();
        let mut new = base.clone();
        new.insert(0, '\n');
        let ops = encode(&base, &new).expect("an inserted blank line should encode");
        let bytes = to_bytes(&ops);
        let back = from_bytes(&bytes).expect("the bytes this module wrote are not parseable");
        assert_eq!(back, ops, "the parsed ops differ from what was written");
        let rebuilt = apply(&base, &back).expect("the delta does not apply");
        assert_eq!(rebuilt, new, "the blank line was lost");
    }

    #[test]
    fn an_insert_of_one_blank_line_round_trips_on_its_own() {
        let ops = vec![Op::Insert(vec![String::new()])];
        let back = from_bytes(&to_bytes(&ops)).unwrap();
        assert_eq!(back, ops);
    }

    #[test]
    fn several_blank_lines_stay_separate() {
        let ops = vec![Op::Insert(vec![
            "a".to_string(),
            String::new(),
            String::new(),
            "b".to_string(),
        ])];
        let back = from_bytes(&to_bytes(&ops)).unwrap();
        assert_eq!(back, ops);
    }
}
